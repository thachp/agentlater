//! Tasks by kind: who made a task, and what follows from that.
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

impl<D: Db> Ledger<'_, D> {
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
             report_failures_only, watch, on_match, action_reserve, triggered_by, created_ms, \
             updated_ms) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, \
             ?, ?, ?, ?, ?, ?, ?)"
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::Memory;
    use crate::ledger::testing::{P, T0};

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
}
