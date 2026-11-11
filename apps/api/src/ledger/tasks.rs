//! Tasks: the `tasks` table, an agent's commands on a task, and what follows from who made it.
//!
//! Every task is made by exactly one of seven makers (an agent's request or watch, a cron's
//! run or alert, a wake, a watch's action, or another task's callback). Its [`Kind`] is fixed
//! when it is made and decides how it is stored (derived ids are inserted idempotently), how
//! it is sent (a watch polls; a wake's body is rewritten), whether it can be redriven, and
//! what its finish sets off. Where its result is reported ([`Reporting`]) is a separate matter.
//!
//! The `tasks` table keeps one nullable column per kind's data. [`TaskRow::kind`] reads them
//! back into one [`Kind`] and refuses a row whose columns name more than one maker.

use super::*;

/// What `tasks.via` holds for a task whose request came through A2A; null for every other.
const A2A: &str = "a2a";
const DEFAULT_PAGE: usize = 20;
const MAX_PAGE: usize = 100;

/// Who made a task, with what that maker stored on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind<'s> {
    /// An agent's schedule_http or queue_http. A lane makes it wait its turn.
    Request { lane: Option<&'s str> },
    /// An agent's watch_http: every attempt is a poll. `settings` and `on_match` are stored JSON.
    Watch {
        settings: &'s str,
        on_match: Option<&'s str>,
    },
    /// One run of the named cron.
    CronRun { cron: &'s str },
    /// An alert about the named cron, posted to its alert URL.
    Alert { cron: &'s str },
    /// The POST to the project's wake URL.
    Wake,
    /// What the watch with this id sent when it matched.
    Action { watch_id: &'s str },
    /// The report of the task with this id finishing, posted to its callback URL.
    Callback { task: &'s str },
}

impl<'s> Kind<'s> {
    /// The fingerprint of a task the ledger makes itself. Agents' tasks carry their request's.
    pub(super) fn fingerprint(self) -> &'static str {
        match self {
            Kind::Request { .. } => "request",
            Kind::Watch { .. } => "watch",
            Kind::CronRun { .. } => "cron",
            Kind::Alert { .. } => "alert",
            Kind::Wake => "wake",
            Kind::Action { .. } => "on_match",
            Kind::Callback { .. } => "callback",
        }
    }

    /// Its id is derived from what caused it, so making it twice is a no-op.
    fn derived_id(self) -> bool {
        matches!(
            self,
            Kind::CronRun { .. } | Kind::Alert { .. } | Kind::Action { .. } | Kind::Callback { .. }
        )
    }

    pub(super) fn is_watch(self) -> bool {
        matches!(self, Kind::Watch { .. })
    }

    pub(super) fn is_wake(self) -> bool {
        self == Kind::Wake
    }

    pub(super) fn lane(self) -> Option<&'s str> {
        match self {
            Kind::Request { lane } => lane,
            _ => None,
        }
    }

    /// Its first attempt is meant to start at `run_at_ms`, so starting later is lateness.
    /// A lane's task waits for the one before it, and a wake is debounced: both late by design.
    pub(super) fn measures_lateness(self) -> bool {
        self.lane().is_none() && !self.is_wake()
    }

    /// Never sent again once finished, and its headers often carry a token: their values
    /// are cleared when it finishes.
    fn scrubbed_when_finished(self) -> bool {
        matches!(self, Kind::Watch { .. } | Kind::Wake)
    }

    /// Why it can't be redriven, if it can't.
    pub(super) fn redrive_refusal(self) -> Option<ApiError> {
        match self {
            Kind::Watch { .. } => Some(ApiError::new(
                409,
                "watch_not_redrivable",
                "a watch cannot be redriven; start a new one",
            )),
            Kind::Wake => Some(ApiError::new(
                409,
                "wake_not_redrivable",
                "a wake cannot be redriven; the next inbox event sends a new one",
            )),
            _ => None,
        }
    }
}

/// Where a finished task's report goes, and what it carries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Reporting<'s> {
    /// A URL, [`INBOX`], or nowhere.
    pub to: Option<&'s str>,
    /// Only if the task fails (a cron run with `report: failures`).
    pub failures_only: bool,
    /// Keep the last response, for get_task and the report.
    pub capture: bool,
}

/// A task to insert.
pub(super) struct NewTask<'s> {
    pub id: &'s str,
    pub kind: Kind<'s>,
    pub task: Task,
    pub method: Method,
    pub url: &'s str,
    pub headers: &'s [(String, String)],
    pub body: Option<String>,
    pub run_at_ms: i64,
    pub idempotency_key: Option<String>,
    pub fingerprint: &'s str,
    pub reporting: Reporting<'s>,
    /// What admission reserved for a watch's action and its callback.
    pub action_reserve: i64,
    /// Whether the request for it came through A2A.
    pub a2a: bool,
}

impl<'s> NewTask<'s> {
    /// A task the ledger makes itself, first due at `task`'s due time: no idempotency key,
    /// no report of its own, nothing reserved.
    pub(super) fn internal(
        id: &'s str,
        kind: Kind<'s>,
        task: Task,
        method: Method,
        url: &'s str,
        headers: &'s [(String, String)],
        body: Option<String>,
    ) -> Self {
        NewTask {
            id,
            kind,
            run_at_ms: task.due_ms,
            task,
            method,
            url,
            headers,
            body,
            idempotency_key: None,
            fingerprint: kind.fingerprint(),
            reporting: Reporting::default(),
            action_reserve: 0,
            a2a: false,
        }
    }
}

/// A kind as the `tasks` columns that hold it.
#[derive(Default)]
struct KindColumns<'s> {
    lane: Option<&'s str>,
    watch: Option<&'s str>,
    on_match: Option<&'s str>,
    cron: Option<&'s str>,
    alert_for: Option<&'s str>,
    wake_call: bool,
    triggered_by: Option<&'s str>,
    callback_for: Option<&'s str>,
}

impl<'s> From<Kind<'s>> for KindColumns<'s> {
    fn from(kind: Kind<'s>) -> Self {
        let none = KindColumns::default();
        match kind {
            Kind::Request { lane } => KindColumns { lane, ..none },
            Kind::Watch { settings, on_match } => KindColumns {
                watch: Some(settings),
                on_match,
                ..none
            },
            Kind::CronRun { cron } => KindColumns {
                cron: Some(cron),
                ..none
            },
            Kind::Alert { cron } => KindColumns {
                alert_for: Some(cron),
                ..none
            },
            Kind::Wake => KindColumns {
                wake_call: true,
                ..none
            },
            Kind::Action { watch_id } => KindColumns {
                triggered_by: Some(watch_id),
                ..none
            },
            Kind::Callback { task } => KindColumns {
                callback_for: Some(task),
                ..none
            },
        }
    }
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct AttemptView {
    pub number: i64,
    pub started_ms: i64,
    pub finished_ms: Option<i64>,
    pub outcome: Option<String>,
    pub http_status: Option<i64>,
}

/// Header values and the body are never echoed back: they may hold credentials.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct TaskView {
    pub id: String,
    pub status: Status,
    pub method: Method,
    pub url: String,
    pub header_names: Vec<String>,
    pub body_bytes: usize,
    pub run_at_ms: i64,
    /// When the next attempt is due, while the task is waiting.
    pub next_attempt_ms: Option<i64>,
    pub max_attempts: u32,
    pub attempt_count: u32,
    pub idempotency_key: Option<String>,
    /// Where this task's final state is POSTed once it finishes.
    pub callback_url: Option<String>,
    /// The callback task for the most recent time this task finished.
    pub callback_task_id: Option<String>,
    /// Set on a callback task: the task it reports on.
    pub callback_for: Option<String>,
    /// With `callback_url: "inbox"`: the inbox event for the most recent time it finished.
    pub inbox_seq: Option<i64>,
    /// Whether the destination's response is kept (see `response`).
    pub capture: bool,
    /// With `capture`: the last response the destination sent.
    pub response: Option<CapturedResponse>,
    /// Set on a wake: the request that told the agent its inbox has something.
    pub wake_call: bool,
    /// Why the task failed without an attempt: secret_missing (a secret its headers refer to
    /// was deleted), secret_host_not_allowed (it no longer allows the host), secret_too_large
    /// (replaced by a value too long for the headers) or secret_unavailable (the API has no
    /// key to open secrets with).
    pub failure: Option<String>,
    /// Set on a run of a cron with `report: failures`: it reports to `callback_url` only if
    /// it fails, so a run that succeeded has no callback task or inbox event, by design.
    pub report_failures_only: bool,
    /// Tasks in the same lane run one at a time.
    pub lane: Option<String>,
    /// Set on a cron's runs: the cron's name.
    pub cron: Option<String>,
    /// Set on a cron alert: the cron it is about.
    pub alert_for: Option<String>,
    /// Set on a watch: what it polls for and how it is going.
    pub watch: Option<WatchView>,
    /// Set on a watch's `on_match` action: the watch that sent it.
    pub triggered_by: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub finished_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempts: Option<Vec<AttemptView>>,
}

/// The last response to a task with `capture`, as stored in `tasks.response`. Comes from the
/// destination, so it is untrusted data.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapturedResponse {
    /// The attempt that got this response.
    pub attempt: u32,
    pub status: u16,
    /// Only [`CAPTURED_HEADERS`], lowercased.
    pub headers: BTreeMap<String, String>,
    /// Up to 16 KiB, as UTF-8 (invalid bytes replaced). Null in lists, when the body could not
    /// be read in time (`truncated` is then true), and in inbox events after 7 days.
    pub body: Option<String>,
    /// The full length, when the destination sent `Content-Length` for an unencoded body.
    pub body_bytes: Option<u64>,
    /// Whether `body` is shorter than what the destination sent.
    pub truncated: bool,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct TaskPage {
    pub tasks: Vec<TaskView>,
    pub next_cursor: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct TaskRow {
    seq: i64,
    pub(super) id: String,
    status: String,
    due_ms: i64,
    max_attempts: u32,
    run_attempts: u32,
    total_attempts: u32,
    pub(super) generation: u64,
    pub(super) fence: u64,
    lease_until_ms: Option<i64>,
    cancel_requested: i64,
    terminal_ms: Option<i64>,
    pub(super) method: String,
    pub(super) url: String,
    headers: String,
    pub(super) body: Option<String>,
    pub(super) run_at_ms: i64,
    idempotency_key: Option<String>,
    callback_url: Option<String>,
    callback_task_id: Option<String>,
    callback_for: Option<String>,
    lane: Option<String>,
    cron: Option<String>,
    alert_for: Option<String>,
    pub(super) watch: Option<String>,
    pub(super) watch_errors: u32,
    pub(super) watch_state: Option<String>,
    capture: i64,
    response: Option<String>,
    inbox_seq: Option<i64>,
    wake_call: i64,
    report_failures_only: i64,
    failure: Option<String>,
    pub(super) on_match: Option<String>,
    triggered_by: Option<String>,
    via: Option<String>,
    pub(super) created_ms: i64,
    updated_ms: i64,
}

#[derive(Deserialize)]
struct AttemptRow {
    number: i64,
    started_ms: i64,
    finished_ms: Option<i64>,
    outcome: Option<String>,
    http_status: Option<i64>,
}

impl TaskRow {
    pub(super) fn task(&self) -> Result<Task, DbError> {
        Ok(Task {
            status: parse(&self.status)?,
            due_ms: self.due_ms,
            max_attempts: self.max_attempts,
            run_attempts: self.run_attempts,
            total_attempts: self.total_attempts,
            generation: self.generation,
            fence: self.fence,
            lease_until_ms: self.lease_until_ms,
            cancel_requested: self.cancel_requested != 0,
            terminal_ms: self.terminal_ms,
        })
    }

    pub(super) fn headers(&self) -> Result<Vec<(String, String)>, DbError> {
        serde_json::from_str(&self.headers).map_err(|e| DbError(e.to_string()))
    }

    /// `full` includes the body; lists leave it out to stay small.
    fn response(&self, full: bool) -> Result<Option<CapturedResponse>, DbError> {
        let Some(stored) = &self.response else {
            return Ok(None);
        };
        let mut response: CapturedResponse =
            serde_json::from_str(stored).map_err(|e| DbError(e.to_string()))?;
        if !full {
            response.body = None;
        }
        Ok(Some(response))
    }

    fn view(&self, attempts: Option<Vec<AttemptView>>) -> Result<TaskView, DbError> {
        let task = self.task()?;
        Ok(TaskView {
            id: self.id.clone(),
            status: task.status,
            method: parse(&self.method)?,
            url: self.url.clone(),
            header_names: self.headers()?.into_iter().map(|(k, _)| k).collect(),
            body_bytes: self.body.as_ref().map_or(0, String::len),
            run_at_ms: self.run_at_ms,
            next_attempt_ms: matches!(task.status, Status::Scheduled | Status::Retrying)
                .then_some(task.due_ms),
            max_attempts: task.max_attempts,
            attempt_count: task.total_attempts,
            idempotency_key: self.idempotency_key.clone(),
            callback_url: self.callback_url.clone(),
            callback_task_id: self.callback_task_id.clone(),
            callback_for: self.callback_for.clone(),
            inbox_seq: self.inbox_seq,
            capture: self.capture != 0,
            response: self.response(attempts.is_some())?,
            wake_call: self.wake_call != 0,
            failure: self.failure.clone(),
            report_failures_only: self.report_failures_only != 0,
            lane: self.lane.clone(),
            cron: self.cron.clone(),
            alert_for: self.alert_for.clone(),
            watch: self.watch_view(attempts.is_some())?,
            triggered_by: self.triggered_by.clone(),
            created_ms: self.created_ms,
            updated_ms: self.updated_ms,
            finished_ms: task.terminal_ms,
            attempts,
        })
    }
}

pub(super) const TASK_COLUMNS: &str = "seq, id, status, due_ms, max_attempts, run_attempts, \
    total_attempts, generation, fence, lease_until_ms, cancel_requested, terminal_ms, \
    method, url, headers, body, run_at_ms, idempotency_key, callback_url, callback_task_id, \
    callback_for, lane, cron, alert_for, watch, watch_errors, watch_state, capture, response, \
    inbox_seq, wake_call, report_failures_only, failure, on_match, triggered_by, via, \
    created_ms, updated_ms";

impl TaskRow {
    /// Who made this task. A row whose columns name more than one maker, or carry data its
    /// maker never sets, is corrupt, and an error rather than a guess.
    pub(super) fn kind(&self) -> Result<Kind<'_>, DbError> {
        let kind = match (
            self.watch.as_deref(),
            self.cron.as_deref(),
            self.alert_for.as_deref(),
            self.wake_call != 0,
            self.triggered_by.as_deref(),
            self.callback_for.as_deref(),
        ) {
            (None, None, None, false, None, None) => Kind::Request {
                lane: self.lane.as_deref(),
            },
            (Some(settings), None, None, false, None, None) => Kind::Watch {
                settings,
                on_match: self.on_match.as_deref(),
            },
            (None, Some(cron), None, false, None, None) => Kind::CronRun { cron },
            (None, None, Some(cron), false, None, None) => Kind::Alert { cron },
            (None, None, None, true, None, None) => Kind::Wake,
            (None, None, None, false, Some(watch_id), None) => Kind::Action { watch_id },
            (None, None, None, false, None, Some(task)) => Kind::Callback { task },
            _ => return Err(self.inconsistent()),
        };
        let stray_lane = self.lane.is_some() && kind.lane().is_none();
        let stray_action = self.on_match.is_some() && !kind.is_watch();
        if stray_lane || stray_action {
            return Err(self.inconsistent());
        }
        Ok(kind)
    }

    fn inconsistent(&self) -> DbError {
        DbError(format!(
            "task {} has task-kind columns that do not fit together",
            self.id
        ))
    }

    pub(super) fn reporting(&self) -> Reporting<'_> {
        Reporting {
            to: self.callback_url.as_deref(),
            failures_only: self.report_failures_only != 0,
            capture: self.capture != 0,
        }
    }
}

/// Callback task ids are derived, so queueing the same callback twice is harmless. A task's
/// generation differs every time it finishes (only a redrive can follow a finish, and it
/// moves the generation on), so each finish gets its own callback.
fn callback_id(task_id: &str, generation: u64) -> String {
    derived_id(&format!("callback/{task_id}/{generation}"))
}

pub(super) fn derived_id(seed: &str) -> String {
    let digest = Sha256::digest(seed);
    format!("task_{}", hex::encode(&digest[..10]))
}

fn no_task() -> ApiError {
    ApiError::not_found("task_not_found", "no such task in this project")
}

impl<D: Db> Ledger<'_, D> {
    /// Returns the task and whether it was created (false for an idempotent replay).
    pub fn schedule(
        &self,
        request: ScheduleRequest,
        task_id: &str,
        now: i64,
    ) -> Result<(TaskView, bool), ApiError> {
        let project = self.project_row()?.ok_or_else(no_project)?;
        let when_field = if request.delay.is_some() {
            "delay"
        } else {
            "run_at_ms"
        };
        let spec = request.validate(now).map_err(ApiError::invalid)?;
        self.check_secrets(&spec.headers, spec.destination.host())?;
        if let Some(existing) = self.replay(spec.idempotency_key.as_deref(), &spec.fingerprint)? {
            return Ok((existing, false));
        }

        let reports_to = spec.callback.as_ref().map(|c| c.as_str());
        self.admit(&project, Cost::task(reports_to), now)?;
        if spec.run_at_ms >= project.expires_ms {
            return Err(ApiError::bad_request(format!(
                "{when_field} is after this project's trial ends"
            ))
            .in_field(when_field));
        }

        self.insert(
            NewTask {
                id: task_id,
                kind: Kind::Request {
                    lane: spec.lane.as_deref(),
                },
                task: Task::new(spec.run_at_ms, spec.max_attempts),
                method: spec.method,
                url: spec.destination.as_str(),
                headers: &spec.headers,
                body: spec.body,
                run_at_ms: spec.run_at_ms,
                idempotency_key: spec.idempotency_key,
                fingerprint: &spec.fingerprint,
                reporting: Reporting {
                    to: spec.callback.as_ref().map(|c| c.as_str()),
                    failures_only: false,
                    capture: spec.capture,
                },
                action_reserve: 0,
                a2a: self.a2a,
            },
            now,
        )?;
        Ok((self.task(task_id)?, true))
    }

    /// The task an idempotency key already made, if any; a different request under the same
    /// key is a conflict, and so, through A2A, is a task that was not made through A2A.
    pub(super) fn replay(
        &self,
        key: Option<&str>,
        fingerprint: &str,
    ) -> Result<Option<TaskView>, ApiError> {
        let Some(key) = key else {
            return Ok(None);
        };
        let existing: Vec<KeyRow> = self.db.query(
            "SELECT id, fingerprint, via FROM tasks WHERE idempotency_key = ?",
            vec![key.into()],
        )?;
        let Some(existing) = existing.first() else {
            return Ok(None);
        };
        if existing.fingerprint != fingerprint || (self.a2a && existing.via.is_none()) {
            return Err(ApiError::new(
                409,
                "idempotency_conflict",
                "idempotency_key was already used for a different request",
            ));
        }
        Ok(Some(self.task(&existing.id)?))
    }

    pub fn cancel(&self, task_id: &str, now: i64) -> Result<TaskView, ApiError> {
        let row = self.named(task_id)?;
        let mut task = row.task()?;
        match task.cancel(now) {
            Ok(_) => {}
            Err(CancelError::AlreadyFinished(s)) => {
                return Err(ApiError::new(
                    409,
                    "task_finished",
                    format!("task already {}", text(&s)),
                ))
            }
        }
        self.save_row(&row, &task, now)?;
        self.task(task_id)
    }

    /// Gives a failed task a fresh set of attempts, starting now.
    pub fn redrive(&self, task_id: &str, now: i64) -> Result<TaskView, ApiError> {
        let project = self.project_row()?.ok_or_else(no_project)?;
        let row = self.named(task_id)?;
        if let Some(refusal) = row.kind()?.redrive_refusal() {
            return Err(refusal);
        }
        let mut task = row.task()?;
        if let Err(RedriveError::NotFailed(s)) = task.redrive(now) {
            return Err(ApiError::new(
                409,
                "task_not_failed",
                format!(
                    "only failed tasks can be redriven; this one is {}",
                    text(&s)
                ),
            ));
        }
        self.admit(&project, Cost::redrive(row.reporting().to), now)?;
        self.save_row(&row, &task, now)?;
        self.db.exec(
            "UPDATE tasks SET failure = NULL WHERE id = ?",
            vec![task_id.into()],
        )?;
        self.task(task_id)
    }

    pub(super) fn task_row(&self, task_id: &str) -> Result<Option<TaskRow>, DbError> {
        Ok(self
            .db
            .query(
                &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?"),
                vec![task_id.into()],
            )?
            .pop())
    }

    /// The task a caller named. Through A2A that is only a task made through A2A: any other
    /// is as absent as one that never existed.
    fn named(&self, task_id: &str) -> Result<TaskRow, ApiError> {
        let row = self.task_row(task_id)?.ok_or_else(no_task)?;
        if self.a2a && row.via.is_none() {
            return Err(no_task());
        }
        Ok(row)
    }

    /// One task with its attempt history.
    pub fn task(&self, task_id: &str) -> Result<TaskView, ApiError> {
        Ok(self.full_view(&self.named(task_id)?)?)
    }

    fn full_view(&self, row: &TaskRow) -> Result<TaskView, DbError> {
        let attempts: Vec<AttemptRow> = self.db.query(
            "SELECT number, started_ms, finished_ms, outcome, http_status FROM attempts \
             WHERE task_id = ? ORDER BY number",
            vec![row.id.as_str().into()],
        )?;
        let attempts = attempts
            .into_iter()
            .map(|a| AttemptView {
                number: a.number,
                started_ms: a.started_ms,
                finished_ms: a.finished_ms,
                outcome: a.outcome,
                http_status: a.http_status,
            })
            .collect();
        row.view(Some(attempts))
    }

    /// Newest first. `cursor` is the `next_cursor` of the previous page.
    pub fn list(
        &self,
        status: Option<Status>,
        lane: Option<&str>,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> Result<TaskPage, ApiError> {
        self.project_row()?.ok_or_else(no_project)?;
        let limit = limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
        let before = match cursor {
            None => MAX_SAFE_INTEGER,
            Some(c) => c
                .parse::<i64>()
                .map_err(|_| ApiError::bad_request("cursor is not valid"))?,
        };
        let mut sql = format!("SELECT {TASK_COLUMNS} FROM tasks WHERE seq < ?");
        let mut params: Vec<Param> = vec![before.into()];
        if self.a2a {
            sql.push_str(" AND via = ?");
            params.push(A2A.into());
        }
        if let Some(status) = status {
            sql.push_str(" AND status = ?");
            params.push(text(&status).into());
        }
        if let Some(lane) = lane {
            if !valid_lane(lane) {
                return Err(ApiError::bad_request("lane is not valid"));
            }
            sql.push_str(" AND lane = ?");
            params.push(lane.into());
        }
        sql.push_str(" ORDER BY seq DESC LIMIT ?");
        params.push((limit as i64 + 1).into());

        let mut rows: Vec<TaskRow> = self.db.query(&sql, params)?;
        let next_cursor = if rows.len() > limit {
            rows.truncate(limit);
            rows.last().map(|r| r.seq.to_string())
        } else {
            None
        };
        let tasks = rows
            .iter()
            .map(|r| r.view(None))
            .collect::<Result<_, _>>()?;
        Ok(TaskPage { tasks, next_cursor })
    }

    pub(super) fn insert(&self, new: NewTask<'_>, now: i64) -> Result<(), DbError> {
        let NewTask { id, task, .. } = &new;
        let headers = serde_json::to_string(new.headers).map_err(|e| DbError(e.to_string()))?;
        let verb = if new.kind.derived_id() {
            "INSERT OR IGNORE"
        } else {
            "INSERT"
        };
        let kind = KindColumns::from(new.kind);
        self.db.exec(
            &format!(
                "{verb} INTO tasks (id, status, due_ms, max_attempts, run_attempts, \
             total_attempts, generation, fence, lease_until_ms, cancel_requested, terminal_ms, \
             published_ms, method, url, headers, body, run_at_ms, idempotency_key, fingerprint, \
             callback_url, callback_for, capture, lane, cron, alert_for, wake_call, \
             report_failures_only, watch, on_match, action_reserve, triggered_by, via, \
             created_ms, updated_ms) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, \
             ?, ?, ?, ?, ?, ?, ?, ?)"
            ),
            vec![
                (*id).into(),
                text(&task.status).into(),
                task.due_ms.into(),
                task.max_attempts.into(),
                task.run_attempts.into(),
                task.total_attempts.into(),
                task.generation.into(),
                task.fence.into(),
                task.lease_until_ms.into(),
                task.cancel_requested.into(),
                task.terminal_ms.into(),
                new.method.as_str().into(),
                new.url.into(),
                headers.into(),
                new.body.into(),
                new.run_at_ms.into(),
                new.idempotency_key.into(),
                new.fingerprint.into(),
                new.reporting.to.into(),
                kind.callback_for.into(),
                new.reporting.capture.into(),
                kind.lane.into(),
                kind.cron.into(),
                kind.alert_for.into(),
                kind.wake_call.into(),
                new.reporting.failures_only.into(),
                kind.watch.into(),
                kind.on_match.into(),
                new.action_reserve.into(),
                kind.triggered_by.into(),
                new.a2a.then_some(A2A).into(),
                now.into(),
                now.into(),
            ],
        )
    }

    /// Saves a change to a task loaded as `row`. If the change finished the task, what its
    /// kind sets off on finishing, and its report, happen in the same transaction.
    pub(super) fn save_row(&self, row: &TaskRow, task: &Task, now: i64) -> Result<(), DbError> {
        self.save(&row.id, task, now)?;
        let was_finished = parse::<Status>(&row.status)?.is_terminal();
        if !task.status.is_terminal() || was_finished {
            return Ok(());
        }
        let kind = row.kind()?;
        match kind {
            Kind::CronRun { cron } => self.run_finished(cron, row, task.status, now)?,
            Kind::Wake => self.wake_finished(&row.id, task.status, now)?,
            Kind::Watch {
                on_match: Some(_), ..
            } => self.watch_ended(row, task, now)?,
            _ => {}
        }
        if kind.scrubbed_when_finished() {
            let names: Vec<(String, String)> = row
                .headers()?
                .into_iter()
                .map(|(name, _)| (name, String::new()))
                .collect();
            let names = serde_json::to_string(&names).map_err(|e| DbError(e.to_string()))?;
            self.db.exec(
                "UPDATE tasks SET headers = ?, body = NULL WHERE id = ?",
                vec![names.into(), row.id.as_str().into()],
            )?;
        }
        let reporting = row.reporting();
        if reporting.failures_only && task.status != Status::Failed {
            return Ok(());
        }
        match reporting.to {
            Some(INBOX) => self.report_to_inbox(&row.id, now),
            Some(url) => self.queue_callback(&row.id, url, task.generation, now),
            None => Ok(()),
        }
    }

    fn save(&self, task_id: &str, task: &Task, now: i64) -> Result<(), DbError> {
        // A new generation is a new schedule: it must be published again. Succeeded and
        // canceled tasks can never be sent again, so their body is dropped; failed tasks
        // keep it for redrive.
        self.db.exec(
            "UPDATE tasks SET status = ?, due_ms = ?, max_attempts = ?, run_attempts = ?, \
             total_attempts = ?, lease_until_ms = ?, cancel_requested = ?, terminal_ms = ?, \
             body = CASE WHEN ? IN ('succeeded', 'canceled') THEN NULL ELSE body END, \
             published_ms = CASE WHEN generation = ? THEN published_ms ELSE NULL END, \
             generation = ?, fence = ?, updated_ms = ? WHERE id = ?",
            vec![
                text(&task.status).into(),
                task.due_ms.into(),
                task.max_attempts.into(),
                task.run_attempts.into(),
                task.total_attempts.into(),
                task.lease_until_ms.into(),
                task.cancel_requested.into(),
                task.terminal_ms.into(),
                text(&task.status).into(),
                task.generation.into(),
                task.generation.into(),
                task.fence.into(),
                now.into(),
                task_id.into(),
            ],
        )
    }

    /// The inbox's version of a callback: the same report, as an event, in this transaction.
    pub(super) fn report_to_inbox(&self, task_id: &str, now: i64) -> Result<(), DbError> {
        self.post(now, |seq| {
            self.db.exec(
                "UPDATE tasks SET inbox_seq = ? WHERE id = ?",
                vec![seq.into(), task_id.into()],
            )?;
            let row = self
                .task_row(task_id)?
                .ok_or_else(|| DbError(format!("task {task_id} vanished")))?;
            Ok(serde_json::json!({ "event": "task.finished", "task": self.full_view(&row)? }))
        })
        .map(|_| ())
    }

    /// A callback is an ordinary task: signed, retried and visible through get_task. Admission
    /// reserved an attempt for it, and [`Self::claim`] gives it one try even past the trial.
    pub(super) fn queue_callback(
        &self,
        task_id: &str,
        url: &str,
        generation: u64,
        now: i64,
    ) -> Result<(), DbError> {
        let id = callback_id(task_id, generation);
        self.db.exec(
            "UPDATE tasks SET callback_task_id = ? WHERE id = ?",
            vec![id.as_str().into(), task_id.into()],
        )?;
        let row = self
            .task_row(task_id)?
            .ok_or_else(|| DbError(format!("task {task_id} vanished")))?;
        let report = serde_json::json!({ "event": "task.finished", "task": self.full_view(&row)? });
        self.insert(
            NewTask::internal(
                &id,
                Kind::Callback { task: task_id },
                Task::new(now, DEFAULT_MAX_ATTEMPTS),
                Method::Post,
                url,
                &[("Content-Type".into(), "application/json".into())],
                Some(report.to_string()),
            ),
            now,
        )
    }
}

#[derive(Deserialize)]
struct KeyRow {
    id: String,
    fingerprint: String,
    via: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::Memory;
    use crate::ledger::testing::*;
    use runlater_core::limits::LEASE_MS;

    fn ledger(db: &Memory) -> Ledger<'_, Memory> {
        let l = Ledger::open(db).unwrap();
        l.create_project(P, "secret", T0).unwrap();
        l
    }

    fn insert(l: &Ledger<'_, Memory>, id: &str, kind: Kind<'_>) {
        l.insert(
            NewTask::internal(
                id,
                kind,
                Task::new(T0, 1),
                Method::Post,
                "https://example.com/",
                &[],
                None,
            ),
            T0,
        )
        .unwrap();
    }

    #[test]
    fn every_kind_reads_back_as_it_was_made() {
        let db = Memory::new();
        let l = ledger(&db);
        let kinds = [
            Kind::Request { lane: None },
            Kind::Request { lane: Some("a") },
            Kind::Watch {
                settings: "{}",
                on_match: None,
            },
            Kind::Watch {
                settings: "{}",
                on_match: Some("{}"),
            },
            Kind::CronRun { cron: "backup" },
            Kind::Alert { cron: "backup" },
            Kind::Wake,
            Kind::Action { watch_id: "t_w" },
            Kind::Callback { task: "t_1" },
        ];
        for (n, kind) in kinds.into_iter().enumerate() {
            let id = format!("t{n}");
            insert(&l, &id, kind);
            assert_eq!(l.task_row(&id).unwrap().unwrap().kind().unwrap(), kind);
        }
    }

    #[test]
    fn a_derived_kind_made_twice_is_one_task() {
        let db = Memory::new();
        let l = ledger(&db);
        insert(&l, "c1", Kind::Callback { task: "t_1" });
        insert(&l, "c1", Kind::Callback { task: "t_1" });
        let count: i64 =
            db.0.query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
                .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn a_row_naming_two_makers_is_an_error() {
        let db = Memory::new();
        let l = ledger(&db);
        insert(&l, "t1", Kind::CronRun { cron: "backup" });
        db.0.execute("UPDATE tasks SET wake_call = 1", []).unwrap();
        assert!(l.task_row("t1").unwrap().unwrap().kind().is_err());

        insert(&l, "t2", Kind::Wake);
        db.0.execute("UPDATE tasks SET lane = 'a' WHERE id = 't2'", [])
            .unwrap();
        assert!(l.task_row("t2").unwrap().unwrap().kind().is_err());

        insert(&l, "t3", Kind::Request { lane: None });
        db.0.execute("UPDATE tasks SET on_match = '{}' WHERE id = 't3'", [])
            .unwrap();
        assert!(l.task_row("t3").unwrap().unwrap().kind().is_err());
    }

    #[test]
    fn derived_ids_are_the_start_of_a_sha256() {
        assert_eq!(derived_id("abc"), "task_ba7816bf8f01cfea4141");
    }

    #[test]
    fn task_routes_need_a_project() {
        let db = Memory::new();
        let l = Ledger::open(&db).unwrap();
        let err = l
            .schedule(req("https://a.example.com/"), "t1", T0)
            .unwrap_err();
        assert_eq!(err.code, "project_not_found");
        assert_eq!(l.list(None, None, None, None).unwrap_err().status, 404);
    }

    #[test]
    fn schedules_and_reads_back_without_secrets() {
        let db = Memory::new();
        let l = setup(&db);
        let mut r = req("https://hooks.example.com/x");
        r.headers
            .insert("Authorization".into(), "Bearer s3cret".into());
        r.body = Some("{\"a\":1}".into());
        let (t, created) = l.schedule(r, "t1", T0).unwrap();
        assert!(created);
        assert_eq!(t.status, Status::Scheduled);
        assert_eq!(t.next_attempt_ms, Some(T0));
        assert_eq!(t.header_names, ["Authorization"]);
        assert_eq!(t.body_bytes, 7);
        assert_eq!(t.attempts, Some(vec![]));
        let json = serde_json::to_string(&t).unwrap();
        assert!(!json.contains("s3cret") && !json.contains("\"a\""));
    }

    #[test]
    fn reports_validation_errors_with_the_field() {
        let db = Memory::new();
        let l = setup(&db);
        let err = l
            .schedule(req("http://a.example.com/"), "t1", T0)
            .unwrap_err();
        assert_eq!(
            (err.status, err.code, err.field),
            (400, "invalid_request", Some("url"))
        );
        assert_eq!(err.body()["error"]["field"], "url");
    }

    #[test]
    fn idempotency_replays_and_conflicts() {
        let db = Memory::new();
        let l = setup(&db);
        let mut r = req("https://a.example.com/");
        r.idempotency_key = Some("k1".into());
        let (first, created) = l.schedule(r.clone(), "t1", T0).unwrap();
        assert!(created);
        let (again, created) = l.schedule(r.clone(), "t2", T0 + 1000).unwrap();
        assert!(!created);
        assert_eq!(again.id, first.id);

        r.body = Some("different".into());
        assert_eq!(
            l.schedule(r, "t3", T0).unwrap_err().code,
            "idempotency_conflict"
        );
    }

    /// The project's ledger as a request that came through A2A sees it.
    fn through_a2a(db: &Memory) -> Ledger<'_, Memory> {
        Ledger::open(db).unwrap().through_a2a(true)
    }

    #[test]
    fn through_a2a_a_task_made_any_other_way_is_as_absent_as_one_that_never_was() {
        let db = Memory::new();
        let l = setup(&db);
        let a2a = through_a2a(&db);
        let (made, created) = a2a
            .schedule(req("https://a.example.com/"), "t1", T0)
            .unwrap();
        assert!(created);
        l.schedule(req("https://a.example.com/"), "t2", T0).unwrap();

        assert_eq!(a2a.task("t1").unwrap(), made);
        let never = a2a.task("t9").unwrap_err();
        assert_eq!((never.status, never.code), (404, "task_not_found"));
        assert_eq!(a2a.task("t2").unwrap_err(), never);
        assert_eq!(a2a.cancel("t2", T0).unwrap_err(), never);
        assert_eq!(l.task("t2").unwrap().status, Status::Scheduled);

        // A list through A2A has only those tasks, and pages over them alone.
        a2a.schedule(req("https://a.example.com/"), "t3", T0)
            .unwrap();
        let ids = |page: &TaskPage| page.tasks.iter().map(|t| t.id.clone()).collect::<Vec<_>>();
        assert_eq!(
            ids(&a2a.list(None, None, None, None).unwrap()),
            ["t3", "t1"]
        );
        let first = a2a.list(None, None, None, Some(1)).unwrap();
        let rest = a2a
            .list(None, None, first.next_cursor.as_deref(), Some(1))
            .unwrap();
        assert_eq!(
            (ids(&first), ids(&rest)),
            (vec!["t3".into()], vec!["t1".to_string()])
        );
        assert_eq!(rest.next_cursor, None);
        assert_eq!(
            ids(&l.list(None, None, None, None).unwrap()),
            ["t3", "t2", "t1"]
        );
    }

    /// Existing callers see every task in the project, as before, and no mark on any.
    #[test]
    fn a_task_made_through_a2a_reads_the_same_as_any_other_from_elsewhere() {
        let db = Memory::new();
        let l = setup(&db);
        let (made, _) = through_a2a(&db)
            .schedule(req("https://a.example.com/"), "t1", T0)
            .unwrap();
        let (plain, _) = l.schedule(req("https://a.example.com/"), "t2", T0).unwrap();
        assert_eq!(l.task("t1").unwrap(), made);
        let (a, b) = (
            serde_json::to_value(&made).unwrap(),
            serde_json::to_value(&plain).unwrap(),
        );
        let keys =
            |v: &serde_json::Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        assert_eq!(keys(&a), keys(&b));
        assert_eq!(l.list(None, None, None, None).unwrap().tasks.len(), 2);
    }

    #[test]
    fn through_a2a_a_key_replays_only_a_task_made_through_a2a() {
        let db = Memory::new();
        let l = setup(&db);
        let a2a = through_a2a(&db);
        let keyed = |key: &str| ScheduleRequest {
            idempotency_key: Some(key.into()),
            ..req("https://a.example.com/")
        };
        let (first, _) = a2a.schedule(keyed("k1"), "t1", T0).unwrap();
        let (again, created) = a2a.schedule(keyed("k1"), "t2", T0 + 1000).unwrap();
        assert_eq!((again.id.as_str(), created), (first.id.as_str(), false));

        // The same request under a key another interface used: its task is not A2A's to return.
        l.schedule(keyed("k2"), "t3", T0).unwrap();
        let conflict = a2a.schedule(keyed("k2"), "t4", T0).unwrap_err();
        assert_eq!(
            (conflict.status, conflict.code),
            (409, "idempotency_conflict")
        );
        assert!(l.task("t4").is_err(), "nothing was scheduled");
        // The other way round is a replay, as between any two existing callers.
        assert_eq!(l.schedule(keyed("k1"), "t5", T0).unwrap().0.id, first.id);
    }

    #[test]
    fn only_the_request_made_through_a2a_is_marked_not_what_the_ledger_makes_for_it() {
        let db = Memory::new();
        setup(&db);
        let a2a = through_a2a(&db);
        a2a.schedule(with_callback("https://a.example.com/"), "t1", T0)
            .unwrap();
        drive(&a2a, "t1", http(200), None, T0);
        let callback = a2a.task("t1").unwrap().callback_task_id.unwrap();
        assert_eq!(a2a.task(&callback).unwrap_err().code, "task_not_found");
    }

    #[test]
    fn delay_errors_name_the_delay_field() {
        let db = Memory::new();
        let l = setup(&db);
        let mut r = req("https://a.example.com/");
        r.delay = Some("soon".into());
        assert_eq!(l.schedule(r, "t1", T0).unwrap_err().field, Some("delay"));

        // Past the trial end: blame the field the caller used.
        db.0.execute("UPDATE project SET expires_ms = ?", [T0 + 3_600_000])
            .unwrap();
        let mut r = req("https://a.example.com/");
        r.delay = Some("2h".into());
        assert_eq!(l.schedule(r, "t2", T0).unwrap_err().field, Some("delay"));
        let mut r = req("https://a.example.com/");
        r.delay = Some("30m".into());
        let (t, _) = l.schedule(r, "t3", T0).unwrap();
        assert_eq!(t.run_at_ms, T0 + 1_800_000);
    }

    #[test]
    fn scheduling_never_silently_drops_a_task() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(req("https://a.example.com/"), "t1", T0).unwrap();
        let err = l
            .schedule(req("https://a.example.com/"), "t1", T0)
            .unwrap_err();
        assert_eq!(err.status, 500, "an id collision is an error, not a 404");
    }

    #[test]
    fn refuses_work_beyond_the_budget_or_trial() {
        let db = Memory::new();
        let l = setup(&db);
        db.0.execute("UPDATE project SET attempt_budget = 2", [])
            .unwrap();
        l.schedule(req("https://a.example.com/"), "t1", T0).unwrap();
        l.schedule(req("https://a.example.com/"), "t2", T0).unwrap();
        let err = l
            .schedule(req("https://a.example.com/"), "t3", T0)
            .unwrap_err();
        assert_eq!((err.status, err.code), (402, "budget_exhausted"));

        let late = T0 + TRIAL_DURATION_MS;
        assert_eq!(
            l.schedule(req("https://a.example.com/"), "t4", late)
                .unwrap_err()
                .code,
            "trial_expired"
        );

        let mut r = req("https://a.example.com/");
        r.run_at_ms = Some(T0 + TRIAL_DURATION_MS);
        db.0.execute("UPDATE project SET attempt_budget = 100", [])
            .unwrap();
        assert_eq!(
            l.schedule(r, "t5", T0).unwrap_err().field,
            Some("run_at_ms")
        );
    }

    #[test]
    fn an_attempt_under_way_is_not_promised_twice() {
        let db = Memory::new();
        let l = setup(&db);
        db.0.execute("UPDATE project SET attempt_budget = 2", [])
            .unwrap();
        l.schedule(req("https://a.example.com/"), "t1", T0).unwrap();
        let d = l.publish_due(P, T0).unwrap().remove(0);
        deliver(&l, &d, T0);
        // t1's attempt is charged and under way; one attempt is left and nothing is waiting.
        l.schedule(req("https://a.example.com/"), "t2", T0).unwrap();
    }

    #[test]
    fn caps_retained_tasks_even_when_canceled() {
        let db = Memory::new();
        let l = setup(&db);
        let mut r = req("https://a.example.com/");
        r.run_at_ms = Some(T0 + 60_000);
        r.body = Some("x".repeat(1024));
        for i in 0..MAX_RETAINED_TASKS {
            let id = format!("t{i}");
            l.schedule(r.clone(), &id, T0).unwrap();
            l.cancel(&id, T0).unwrap();
        }
        let err = l.schedule(r, "one_more", T0).unwrap_err();
        assert_eq!((err.status, err.code), (429, "task_limit"));

        // Purge frees the space once finished tasks age out.
        l.purge(T0 + RETENTION_MS).unwrap();
        let total: i64 =
            db.0.query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
                .unwrap();
        assert_eq!(total, 0);
    }

    #[test]
    fn admission_reserves_an_attempt_and_a_row_for_the_callback() {
        let db = Memory::new();
        let l = setup(&db);
        db.0.execute("UPDATE project SET attempt_budget = 3", [])
            .unwrap();
        l.schedule(with_callback("https://a.example.com/"), "t1", T0)
            .unwrap();
        let err = l
            .schedule(with_callback("https://a.example.com/"), "t2", T0)
            .unwrap_err();
        assert_eq!(err.code, "budget_exhausted");
        l.schedule(req("https://a.example.com/"), "t3", T0).unwrap();

        // Abandoned for lack of budget still reports, and the callback gets one last try.
        db.0.execute("UPDATE project SET attempts_used = 3", [])
            .unwrap();
        let d = l
            .publish_due(P, T0)
            .unwrap()
            .into_iter()
            .find(|d| d.task_id == "t1")
            .unwrap();
        assert_eq!(
            l.claim(&d.task_id, d.generation, T0).unwrap(),
            Claimed::Skip("budget_exhausted")
        );
        let cb_id = l.task("t1").unwrap().callback_task_id.unwrap();
        let d = l
            .publish_due(P, T0 + REPUBLISH_MS)
            .unwrap()
            .into_iter()
            .find(|d| d.task_id == cb_id)
            .unwrap();
        let attempt = deliver(&l, &d, T0 + REPUBLISH_MS);
        assert_eq!(
            l.finish(&cb_id, attempt.fence, http(503), T0 + REPUBLISH_MS, 0.0)
                .unwrap(),
            Some(Status::Failed),
            "no retries past the budget"
        );
        assert_eq!(l.project(T0).unwrap().attempts_remaining, 0);

        // Near the retained-task cap a callback needs a second row.
        db.0.execute(
            "UPDATE project SET attempt_budget = 100, attempts_used = 0",
            [],
        )
        .unwrap();
        let mut later = req("https://a.example.com/");
        later.run_at_ms = Some(T0 + 60_000);
        let held: i64 =
            db.0.query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
                .unwrap();
        for i in held..MAX_RETAINED_TASKS - 1 {
            let id = format!("x{i}");
            l.schedule(later.clone(), &id, T0).unwrap();
            l.cancel(&id, T0).unwrap();
        }
        let err = l
            .schedule(with_callback("https://a.example.com/"), "big", T0)
            .unwrap_err();
        assert_eq!(err.code, "task_limit");
        l.schedule(later, "small", T0).unwrap();
    }

    #[test]
    fn owed_callbacks_count_against_the_task_cap() {
        let db = Memory::new();
        let l = setup(&db);
        let mut later = req("https://a.example.com/");
        later.run_at_ms = Some(T0 + 60_000);
        let owes = ScheduleRequest {
            run_at_ms: later.run_at_ms,
            ..with_callback("https://a.example.com/")
        };
        l.schedule(owes, "w", T0).unwrap();
        for i in 0..MAX_RETAINED_TASKS - 2 {
            let id = format!("x{i}");
            l.schedule(later.clone(), &id, T0).unwrap();
            l.cancel(&id, T0).unwrap();
        }
        let err = l.schedule(later, "one_more", T0).unwrap_err();
        assert_eq!(err.code, "task_limit", "w's callback still needs a row");
    }

    #[test]
    fn drops_the_body_once_a_task_cannot_be_sent_again() {
        let db = Memory::new();
        let l = setup(&db);
        let body = |id: &str| -> Option<String> {
            db.0.query_row("SELECT body FROM tasks WHERE id = ?", [id], |r| r.get(0))
                .unwrap()
        };
        let mut r = req("https://a.example.com/");
        r.body = Some("payload".into());
        r.max_attempts = Some(1);

        let mut later = r.clone();
        later.run_at_ms = Some(T0 + 60_000);
        l.schedule(later, "canceled", T0).unwrap();
        l.cancel("canceled", T0).unwrap();
        assert_eq!(body("canceled"), None);

        l.schedule(r.clone(), "ok", T0).unwrap();
        l.schedule(r, "bad", T0).unwrap();
        for d in l.publish_due(P, T0).unwrap() {
            let status = if d.task_id == "ok" { 200 } else { 500 };
            let attempt = deliver(&l, &d, T0);
            l.finish(&d.task_id, attempt.fence, http(status), T0, 0.0)
                .unwrap();
        }
        assert_eq!(body("ok"), None);
        assert_eq!(body("bad").as_deref(), Some("payload"), "kept for redrive");
        l.redrive("bad", T0 + 1).unwrap();
        let d = &l.publish_due(P, T0 + 1).unwrap()[0];
        assert_eq!(deliver(&l, d, T0 + 1).body.as_deref(), Some("payload"));
    }

    #[test]
    fn cancel_and_redrive() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(req("https://a.example.com/"), "t1", T0 + 60_000)
            .unwrap();
        assert_eq!(l.cancel("t1", T0).unwrap().status, Status::Canceled);
        assert_eq!(l.redrive("t1", T0).unwrap_err().code, "task_not_failed");
        assert_eq!(l.cancel("nope", T0).unwrap_err().code, "task_not_found");

        let mut r = req("https://a.example.com/");
        r.max_attempts = Some(1);
        l.schedule(r, "t2", T0).unwrap();
        let due = l.publish_due(P, T0).unwrap();
        deliver(&l, &due[0], T0);
        l.finish("t2", 1, http(500), T0, 0.0).unwrap();
        assert_eq!(l.cancel("t2", T0).unwrap_err().code, "task_finished");

        let t = l.redrive("t2", T0 + 1).unwrap();
        assert_eq!(t.status, Status::Scheduled);
        let due = l.publish_due(P, T0 + 1).unwrap();
        assert_eq!(due[0].generation, 2);
        assert_eq!(deliver(&l, &due[0], T0 + 1).attempt, 2);
    }

    #[test]
    fn lists_newest_first_with_cursor_and_filter() {
        let db = Memory::new();
        let l = setup(&db);
        for i in 0..5 {
            l.schedule(req("https://a.example.com/"), &format!("t{i}"), T0 + 60_000)
                .unwrap();
        }
        l.cancel("t3", T0).unwrap();

        let page = l.list(None, None, None, Some(2)).unwrap();
        let ids: Vec<_> = page.tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["t4", "t3"]);
        assert!(page.tasks[0].attempts.is_none());
        let page2 = l
            .list(None, None, page.next_cursor.as_deref(), Some(2))
            .unwrap();
        assert_eq!(page2.tasks[0].id, "t2");
        let last = l
            .list(None, None, page2.next_cursor.as_deref(), Some(2))
            .unwrap();
        assert_eq!((last.tasks.len(), last.next_cursor), (1, None));

        let canceled = l.list(Some(Status::Canceled), None, None, None).unwrap();
        assert_eq!(canceled.tasks.len(), 1);
        assert_eq!(l.list(None, None, Some("x"), None).unwrap_err().status, 400);
    }

    #[test]
    fn lists_by_lane() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(in_lane("pay", T0), "a", T0).unwrap();
        l.schedule(in_lane("mail", T0), "b", T0).unwrap();
        let page = l.list(None, Some("pay"), None, None).unwrap();
        let ids: Vec<_> = page.tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["a"]);
        assert_eq!(
            l.list(None, Some("a b"), None, None).unwrap_err().status,
            400
        );
    }

    #[test]
    fn a_finished_task_posts_its_result_to_the_callback() {
        let db = Memory::new();
        let l = setup(&db);
        let (t, _) = l
            .schedule(with_callback("https://a.example.com/"), "t1", T0)
            .unwrap();
        assert_eq!(
            t.callback_url.as_deref(),
            Some("https://cb.example.com/done")
        );
        assert_eq!(t.callback_task_id, None);

        let d = &l.publish_due(P, T0).unwrap()[0];
        let a = deliver(&l, d, T0);
        l.finish("t1", a.fence, http(204), T0 + 5, 0.0).unwrap();

        let t = l.task("t1").unwrap();
        let cb_id = t.callback_task_id.expect("callback queued");
        assert!(cb_id.starts_with("task_") && cb_id.len() == 25, "{cb_id}");
        let cb = l.task(&cb_id).unwrap();
        assert_eq!(cb.callback_for.as_deref(), Some("t1"));
        assert_eq!((cb.status, cb.method), (Status::Scheduled, Method::Post));
        assert_eq!(cb.url, "https://cb.example.com/done");
        assert_eq!(cb.callback_url, None, "callbacks do not call back");

        let d = &l.publish_due(P, T0 + 5).unwrap()[0];
        assert_eq!(d.task_id, cb_id);
        let delivery = deliver(&l, d, T0 + 5);
        assert_eq!(
            delivery.headers,
            [("Content-Type".to_string(), "application/json".to_string())]
        );
        let report: serde_json::Value =
            serde_json::from_str(delivery.body.as_deref().unwrap()).unwrap();
        assert_eq!(report["event"], "task.finished");
        assert_eq!(report["task"]["id"], "t1");
        assert_eq!(report["task"]["status"], "succeeded");
        assert_eq!(report["task"]["callback_task_id"], cb_id.as_str());
        assert_eq!(report["task"]["attempts"][0]["http_status"], 204);

        l.finish(&cb_id, delivery.fence, http(200), T0 + 6, 0.0)
            .unwrap();
        assert_eq!(l.task(&cb_id).unwrap().status, Status::Succeeded);
        assert_eq!(l.next_wake().unwrap(), Some(T0 + 5 + RETENTION_MS));
        assert_eq!(l.project(T0).unwrap().attempts_used, 2);
    }

    #[test]
    fn every_way_of_finishing_calls_back_once() {
        let db = Memory::new();
        let l = setup(&db);
        let callbacks = |id: &str| -> i64 {
            db.0.query_row(
                "SELECT COUNT(*) FROM tasks WHERE callback_for = ?",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        let mut later = with_callback("https://a.example.com/");
        later.run_at_ms = Some(T0 + 60_000);

        // Canceled while waiting; canceling again does not call back twice.
        l.schedule(later, "c", T0).unwrap();
        l.cancel("c", T0).unwrap();
        l.cancel("c", T0 + 1).unwrap();
        assert_eq!(callbacks("c"), 1);

        // Failed, redriven and failed again: one callback per finish.
        let mut once = with_callback("https://a.example.com/");
        once.max_attempts = Some(1);
        l.schedule(once, "f", T0).unwrap();
        for now in [T0, T0 + 10] {
            let d = l
                .publish_due(P, now)
                .unwrap()
                .into_iter()
                .find(|d| d.task_id == "f")
                .unwrap();
            let a = deliver(&l, &d, now);
            l.finish("f", a.fence, http(500), now, 0.0).unwrap();
            if now == T0 {
                assert_eq!(callbacks("f"), 1);
                l.redrive("f", T0 + 10).unwrap();
            }
        }
        assert_eq!(callbacks("f"), 2);

        // Lost attempt that exhausts the task's attempts.
        let mut lost = with_callback("https://a.example.com/");
        lost.max_attempts = Some(1);
        l.schedule(lost, "l", T0).unwrap();
        let d = l
            .publish_due(P, T0 + 20)
            .unwrap()
            .into_iter()
            .find(|d| d.task_id == "l")
            .unwrap();
        deliver(&l, &d, T0 + 20);
        l.recover(T0 + 20 + LEASE_MS, || 0.0).unwrap();
        assert_eq!(l.task("l").unwrap().status, Status::Failed);
        assert_eq!(callbacks("l"), 1);
    }

    #[test]
    fn callback_ids_are_stable_and_distinct() {
        assert_eq!(callback_id("t1", 1), callback_id("t1", 1));
        assert_ne!(callback_id("t1", 1), callback_id("t1", 2));
        assert_ne!(callback_id("t1", 1), callback_id("t2", 1));
    }
}
