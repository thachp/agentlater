//! One project's task ledger: the authoritative store for its trial, tasks and attempts.
//!
//! Each project lives in its own Durable Object with its own SQLite database, so nothing
//! here filters by project. Every method is synchronous; in a Durable Object a run of
//! storage calls with no `await` between them commits atomically.
//!
//! The task rows double as the outbox: a due task stays eligible for publication to the
//! queue until a consumer claims it, so a lost queue message is simply republished.

use crate::db::{Db, DbError, Param};
use runlater_core::limits::{
    CAPTURED_HEADERS, CAPTURE_BODY_BYTES, CAPTURE_RETENTION_MS, CREDIT_VALIDITY_MS,
    DEFAULT_MAX_ATTEMPTS, INBOX_ACKED_RETENTION_MS, INBOX_DEFAULT_PAGE, INBOX_MAX_PAGE,
    MAX_CREDIT_ATTEMPTS, MAX_CRONS, MAX_HEADER_BYTES, MAX_INBOX_EVENTS, MAX_RETAINED_TASKS,
    MAX_RETAINED_TASKS_PAID, MAX_SECRETS, MAX_WAKE_FAILURES, RETENTION_MS, TRIAL_ATTEMPTS,
    TRIAL_DURATION_MS, WATCH_EXCERPT_BYTES,
};
use runlater_core::{
    fill, header_references, host_allowed, judge, next_poll, valid_lane, Attempt, Callback,
    CancelError, ClaimError, CronRequest, CronRequestError, Method, Outcome, Poll, PollBody,
    RedriveError, Report, Schedule, ScheduleRequest, Status, Task, Until, ValidationError,
    WakeRequest, WakeRequestError, Watch, WatchRequest, WatchRequestError, INBOX,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// A published task that has not been claimed after this long is published again.
pub const REPUBLISH_MS: i64 = 60_000;
/// Most tasks published per alarm; the alarm re-arms immediately if more are due.
pub const PUBLISH_BATCH: usize = 100;
pub const DEFAULT_PAGE: usize = 20;
pub const MAX_PAGE: usize = 100;
/// Durable Object SQL bindings pass through JavaScript numbers.
const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS project (
        id TEXT PRIMARY KEY,
        created_ms INTEGER NOT NULL,
        expires_ms INTEGER NOT NULL,
        attempt_budget INTEGER NOT NULL,
        attempts_used INTEGER NOT NULL,
        signing_secret TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS tasks (
        seq INTEGER PRIMARY KEY,
        id TEXT NOT NULL UNIQUE,
        status TEXT NOT NULL,
        due_ms INTEGER NOT NULL,
        max_attempts INTEGER NOT NULL,
        run_attempts INTEGER NOT NULL,
        total_attempts INTEGER NOT NULL,
        generation INTEGER NOT NULL,
        fence INTEGER NOT NULL,
        lease_until_ms INTEGER,
        cancel_requested INTEGER NOT NULL,
        terminal_ms INTEGER,
        published_ms INTEGER,
        method TEXT NOT NULL,
        url TEXT NOT NULL,
        headers TEXT NOT NULL,
        body TEXT,
        run_at_ms INTEGER NOT NULL,
        idempotency_key TEXT UNIQUE,
        fingerprint TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS tasks_by_status_due ON tasks (status, due_ms)",
    "CREATE TABLE IF NOT EXISTS attempts (
        task_id TEXT NOT NULL,
        number INTEGER NOT NULL,
        started_ms INTEGER NOT NULL,
        finished_ms INTEGER,
        outcome TEXT,
        http_status INTEGER,
        PRIMARY KEY (task_id, number)
    )",
    "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL)",
];

/// Changes to ledgers created by an earlier [`SCHEMA`]. Each entry runs once per ledger, in
/// order; how many have run is kept in `meta`. Append only.
const MIGRATIONS: &[&[&str]] = &[
    // 1: callbacks.
    &[
        "ALTER TABLE tasks ADD COLUMN callback_url TEXT",
        "ALTER TABLE tasks ADD COLUMN callback_task_id TEXT",
        "ALTER TABLE tasks ADD COLUMN callback_for TEXT",
    ],
    // 2: lanes.
    &[
        "ALTER TABLE tasks ADD COLUMN lane TEXT",
        // Covers LANE_CLEAR's lookup, so finding a lane's head stays a seek.
        "CREATE INDEX IF NOT EXISTS tasks_by_lane ON tasks (lane, status, due_ms, seq)",
    ],
    // 3: crons.
    &[
        "CREATE TABLE IF NOT EXISTS crons (
            name TEXT PRIMARY KEY,
            schedule TEXT NOT NULL,
            method TEXT NOT NULL,
            url TEXT NOT NULL,
            headers TEXT NOT NULL,
            body TEXT,
            max_attempts INTEGER NOT NULL,
            alert_url TEXT,
            next_run_ms INTEGER,
            health TEXT NOT NULL,
            health_reason TEXT,
            last_run_task_id TEXT,
            last_run_ms INTEGER,
            last_success_ms INTEGER,
            failing_since_ms INTEGER,
            health_run_at_ms INTEGER,
            created_ms INTEGER NOT NULL,
            updated_ms INTEGER NOT NULL
        )",
        "ALTER TABLE tasks ADD COLUMN cron TEXT",
        "ALTER TABLE tasks ADD COLUMN alert_for TEXT",
    ],
    // 4: watches.
    &[
        "ALTER TABLE tasks ADD COLUMN watch TEXT",
        "ALTER TABLE tasks ADD COLUMN watch_errors INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE tasks ADD COLUMN watch_state TEXT",
    ],
    // 5: bought attempts. One row per payment, so a repeated webhook adds nothing.
    &["CREATE TABLE IF NOT EXISTS payments (
            id TEXT PRIMARY KEY,
            attempts INTEGER NOT NULL,
            created_ms INTEGER NOT NULL
        )"],
    // 6: response capture and the inbox. The project row holds the inbox's counters: the last
    // seq handed out (seqs are never reused, even after events are removed), the read cursor,
    // and how many unread events were removed since the cursor last moved.
    &[
        "ALTER TABLE tasks ADD COLUMN capture INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE tasks ADD COLUMN response TEXT",
        "ALTER TABLE tasks ADD COLUMN inbox_seq INTEGER",
        "ALTER TABLE project ADD COLUMN inbox_last_seq INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE project ADD COLUMN inbox_acked_seq INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE project ADD COLUMN inbox_dropped INTEGER NOT NULL DEFAULT 0",
        // response_until_ms: when the captured response body inside the event must go,
        // in step with the task's own copy.
        "CREATE TABLE IF NOT EXISTS inbox (
            seq INTEGER PRIMARY KEY,
            at_ms INTEGER NOT NULL,
            event TEXT NOT NULL,
            response_until_ms INTEGER
        )",
    ],
    // 7: wake. At most one row: where to send a wake, and how it is going. covered_seq is the
    // newest inbox event the last wake told the agent about.
    &[
        "CREATE TABLE IF NOT EXISTS wake (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            url TEXT NOT NULL,
            headers TEXT NOT NULL,
            debounce_ms INTEGER NOT NULL,
            state TEXT NOT NULL,
            disabled_reason TEXT,
            consecutive_failures INTEGER NOT NULL,
            last_wake_ms INTEGER,
            last_wake_task_id TEXT,
            covered_seq INTEGER NOT NULL,
            created_ms INTEGER NOT NULL,
            updated_ms INTEGER NOT NULL
        )",
        "ALTER TABLE tasks ADD COLUMN wake_call INTEGER NOT NULL DEFAULT 0",
        // Numbers wake tasks. On the project, so deleting and setting the wake never reuses one.
        "ALTER TABLE project ADD COLUMN wakes_queued INTEGER NOT NULL DEFAULT 0",
    ],
    // 8: crons that report. Each run inherits the cron's callback_url and capture;
    // report_failures_only holds back a run's report unless it failed.
    &[
        "ALTER TABLE crons ADD COLUMN callback_url TEXT",
        "ALTER TABLE crons ADD COLUMN capture INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE crons ADD COLUMN report TEXT NOT NULL DEFAULT 'always'",
        "ALTER TABLE tasks ADD COLUMN report_failures_only INTEGER NOT NULL DEFAULT 0",
    ],
    // 9: secrets. `sealed` is ciphertext from the API's sealing key; the ledger never sees a
    // value. `failure` says why a task failed without an attempt (a secret it needed).
    &[
        "CREATE TABLE IF NOT EXISTS secrets (
            name TEXT PRIMARY KEY,
            sealed TEXT NOT NULL,
            value_bytes INTEGER NOT NULL,
            allowed_hosts TEXT NOT NULL,
            created_ms INTEGER NOT NULL,
            updated_ms INTEGER NOT NULL
        )",
        "ALTER TABLE tasks ADD COLUMN failure TEXT",
    ],
];

/// SQL condition, over `tasks`: the task will queue a callback task when it finishes.
/// Inbox reports are not tasks, so they need no attempt or row of their own.
const OWES_CALLBACK: &str = "(callback_url IS NOT NULL AND callback_url != 'inbox')";

/// SQL condition, over `tasks t`: the task may run now as far as its lane is concerned.
/// A lane runs one task at a time. A task that has started (delivering, or retrying after a
/// failed attempt) holds the lane until it finishes; otherwise the waiting task with the
/// earliest due time, then the earliest scheduled, goes next. Finished tasks never block.
const LANE_CLEAR: &str =
    "(t.lane IS NULL OR t.status IN ('delivering', 'retrying') OR NOT EXISTS ( \
    SELECT 1 FROM tasks o WHERE o.lane = t.lane AND o.seq != t.seq AND ( \
    o.status IN ('delivering', 'retrying') OR (o.status = 'scheduled' AND \
    (o.due_ms < t.due_ms OR (o.due_ms = t.due_ms AND o.seq < t.seq))))))";

// ---------------------------------------------------------------------------------------
// Errors

#[derive(Debug, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub field: Option<&'static str>,
}

impl ApiError {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            field: None,
        }
    }

    pub fn invalid_cron(e: CronRequestError) -> Self {
        Self {
            field: Some(e.field()),
            ..Self::new(400, "invalid_request", e.to_string())
        }
    }

    pub fn invalid_watch(e: WatchRequestError) -> Self {
        Self {
            field: Some(e.field()),
            ..Self::new(400, "invalid_request", e.to_string())
        }
    }

    pub fn invalid(e: ValidationError) -> Self {
        Self {
            field: Some(e.field()),
            ..Self::new(400, "invalid_request", e.to_string())
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request", message)
    }

    pub fn not_found(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(404, code, message)
    }

    pub fn body(&self) -> serde_json::Value {
        let mut e = serde_json::json!({ "code": self.code, "message": self.message });
        if let Some(field) = self.field {
            e["field"] = field.into();
        }
        serde_json::json!({ "error": e })
    }
}

impl From<DbError> for ApiError {
    fn from(e: DbError) -> Self {
        Self::new(500, "internal_error", e.to_string())
    }
}

fn no_project() -> ApiError {
    ApiError::not_found("project_not_found", "create the project first")
}

fn no_task() -> ApiError {
    ApiError::not_found("task_not_found", "no such task in this project")
}

// ---------------------------------------------------------------------------------------
// Views returned to callers

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectView {
    pub id: String,
    pub created_ms: i64,
    pub expires_ms: i64,
    pub attempt_budget: i64,
    pub attempts_used: i64,
    pub attempts_remaining: i64,
    /// Whether attempts have ever been bought for this project.
    pub paid: bool,
    /// Receivers verify `RunLater-Signature` with this.
    pub signing_secret: String,
    /// Inbox events not yet acked; `read_inbox` returns them.
    #[serde(default)]
    pub inbox_unread: i64,
    /// Where RunLater sends a wake when the inbox gets something; see `set_wake`.
    #[serde(default)]
    pub wake: Option<WakeView>,
}

/// Header values are never echoed back, as for tasks.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WakeView {
    pub url: String,
    pub header_names: Vec<String>,
    pub debounce_ms: i64,
    /// active, or disabled (see `disabled_reason`) after wakes kept failing.
    pub state: String,
    pub disabled_reason: Option<String>,
    /// Wakes in a row that failed; three turn the wake off.
    pub consecutive_failures: i64,
    /// When the last wake was sent; the next goes no sooner than `debounce_ms` after it.
    pub last_wake_ms: Option<i64>,
    /// The latest wake task: pending, being sent, or finished.
    pub last_wake_task_id: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
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
    pub created_ms: i64,
    pub updated_ms: i64,
    pub finished_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempts: Option<Vec<AttemptView>>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct WatchView {
    pub every_ms: i64,
    pub deadline_ms: i64,
    pub until: Until,
    /// Polls in a row that failed (5xx, timeout, network); three end the watch.
    pub consecutive_errors: u32,
    /// Why the watch ended: matched, timed_out, poll_failing, poll_rejected,
    /// response_too_large or budget_exhausted. None while it is still polling, or if canceled.
    pub reason: Option<String>,
    /// The value that satisfied `until`.
    pub matched: Option<serde_json::Value>,
    /// The last poll's response status, and up to 16 KiB of its body.
    pub last_status: Option<u16>,
    pub last_body: Option<String>,
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

/// A response the consumer read for a watch's poll or a task's capture.
#[derive(Clone, Copy, Debug)]
pub struct ResponseRead<'a> {
    /// None when the body could not be read in time; a capture then keeps the status and
    /// headers alone.
    pub body: Option<PollBody<'a>>,
    /// Response headers the consumer kept: [`CAPTURED_HEADERS`], `content-length` and
    /// `content-encoding`.
    pub headers: &'a [(String, String)],
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct InboxPage {
    /// Oldest first: `{seq, at_ms, event, ...}` with the same payload a callback or alert sends.
    pub events: Vec<serde_json::Value>,
    pub acked_seq: i64,
    pub unread: i64,
    /// Unread events removed (by age, or because the inbox was full) since the cursor last moved.
    pub dropped: i64,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct InboxCursor {
    pub acked_seq: i64,
    pub unread: i64,
}

/// A watch's progress as stored in `tasks.watch_state`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct WatchState {
    reason: Option<String>,
    matched: Option<serde_json::Value>,
    last_status: Option<u16>,
    last_body: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct TaskPage {
    pub tasks: Vec<TaskView>,
    pub next_cursor: Option<String>,
}

/// Queue message: which task to try, and for which schedule of it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Dispatch {
    pub project_id: String,
    pub task_id: String,
    pub generation: u64,
}

/// Everything the consumer needs to make one attempt.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Delivery {
    pub task_id: String,
    pub attempt: u32,
    pub fence: u64,
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub signing_secret: String,
    /// A watch's poll: the consumer reads the response body and reports it with the outcome.
    #[serde(default)]
    pub read_body: bool,
    /// A task with `capture`: the consumer reads the start of the body and a few headers, but
    /// never lets a failed read change the outcome.
    #[serde(default)]
    pub capture: bool,
    /// The secrets `headers` refer to, still sealed. The consumer opens them, fills them in
    /// just before sending, and redacts their values from any response it reports.
    #[serde(default)]
    pub secrets: Vec<SealedSecret>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SealedSecret {
    pub name: String,
    pub sealed: String,
}

/// A secret as callers see it: never its value.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct SecretView {
    pub name: String,
    pub allowed_hosts: Vec<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct SecretList {
    pub secrets: Vec<SecretView>,
}

#[derive(Deserialize)]
struct SecretRow {
    name: String,
    sealed: String,
    /// The value's length, so headers can be sized as they will be sent.
    value_bytes: i64,
    allowed_hosts: String,
    created_ms: i64,
    updated_ms: i64,
}

impl SecretRow {
    fn hosts(&self) -> Result<Vec<String>, DbError> {
        serde_json::from_str(&self.allowed_hosts).map_err(|e| DbError(e.to_string()))
    }

    fn view(&self) -> Result<SecretView, DbError> {
        Ok(SecretView {
            name: self.name.clone(),
            allowed_hosts: self.hosts()?,
            created_ms: self.created_ms,
            updated_ms: self.updated_ms,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Claimed {
    Deliver(Box<Delivery>),
    /// Nothing to do for this message; acknowledge it.
    Skip(&'static str),
}

// ---------------------------------------------------------------------------------------
// Rows

#[derive(Deserialize)]
struct ProjectRow {
    id: String,
    created_ms: i64,
    expires_ms: i64,
    attempt_budget: i64,
    attempts_used: i64,
    signing_secret: String,
    paid: i64,
    inbox_last_seq: i64,
    inbox_acked_seq: i64,
    inbox_dropped: i64,
    inbox_unread: i64,
}

impl ProjectRow {
    fn view(self) -> ProjectView {
        ProjectView {
            attempts_remaining: (self.attempt_budget - self.attempts_used).max(0),
            id: self.id,
            created_ms: self.created_ms,
            expires_ms: self.expires_ms,
            attempt_budget: self.attempt_budget,
            attempts_used: self.attempts_used,
            paid: self.paid != 0,
            signing_secret: self.signing_secret,
            inbox_unread: self.inbox_unread,
            wake: None,
        }
    }

    fn max_retained_tasks(&self) -> i64 {
        if self.paid != 0 {
            MAX_RETAINED_TASKS_PAID
        } else {
            MAX_RETAINED_TASKS
        }
    }

    fn can_attempt(&self, now: i64) -> bool {
        now < self.expires_ms && self.attempts_used < self.attempt_budget
    }
}

#[derive(Deserialize)]
struct TaskRow {
    seq: i64,
    id: String,
    status: String,
    due_ms: i64,
    max_attempts: u32,
    run_attempts: u32,
    total_attempts: u32,
    generation: u64,
    fence: u64,
    lease_until_ms: Option<i64>,
    cancel_requested: i64,
    terminal_ms: Option<i64>,
    method: String,
    url: String,
    headers: String,
    body: Option<String>,
    run_at_ms: i64,
    idempotency_key: Option<String>,
    callback_url: Option<String>,
    callback_task_id: Option<String>,
    callback_for: Option<String>,
    lane: Option<String>,
    cron: Option<String>,
    alert_for: Option<String>,
    watch: Option<String>,
    watch_errors: u32,
    watch_state: Option<String>,
    capture: i64,
    response: Option<String>,
    inbox_seq: Option<i64>,
    wake_call: i64,
    report_failures_only: i64,
    failure: Option<String>,
    created_ms: i64,
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

fn parse<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, DbError> {
    serde_json::from_value(serde_json::Value::String(s.into()))
        .map_err(|_| DbError(format!("unexpected stored value {s:?}")))
}

fn text<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        _ => unreachable!("enums serialize as strings"),
    }
}

impl TaskRow {
    fn task(&self) -> Result<Task, DbError> {
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

    fn headers(&self) -> Result<Vec<(String, String)>, DbError> {
        serde_json::from_str(&self.headers).map_err(|e| DbError(e.to_string()))
    }

    fn watch(&self) -> Result<Option<Watch>, DbError> {
        self.watch
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|e| DbError(e.to_string()))
    }

    fn watch_state(&self) -> Result<WatchState, DbError> {
        Ok(match &self.watch_state {
            Some(s) => serde_json::from_str(s).map_err(|e| DbError(e.to_string()))?,
            None => WatchState::default(),
        })
    }

    /// `full` includes the last response body; lists leave it out to stay small.
    fn watch_view(&self, full: bool) -> Result<Option<WatchView>, DbError> {
        let Some(watch) = self.watch()? else {
            return Ok(None);
        };
        let state = self.watch_state()?;
        Ok(Some(WatchView {
            every_ms: watch.every_ms,
            deadline_ms: watch.deadline_ms,
            until: watch.until,
            consecutive_errors: self.watch_errors,
            reason: state.reason,
            matched: state.matched,
            last_status: state.last_status,
            last_body: state.last_body.filter(|_| full),
        }))
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
            created_ms: self.created_ms,
            updated_ms: self.updated_ms,
            finished_ms: task.terminal_ms,
            attempts,
        })
    }
}

const TASK_COLUMNS: &str = "seq, id, status, due_ms, max_attempts, run_attempts, \
    total_attempts, generation, fence, lease_until_ms, cancel_requested, terminal_ms, \
    method, url, headers, body, run_at_ms, idempotency_key, callback_url, callback_task_id, \
    callback_for, lane, cron, alert_for, watch, watch_errors, watch_state, capture, response, \
    inbox_seq, wake_call, report_failures_only, failure, created_ms, updated_ms";

/// A row to insert into `tasks`.
struct NewTask<'s> {
    id: &'s str,
    task: Task,
    method: Method,
    url: &'s str,
    headers: &'s [(String, String)],
    body: Option<String>,
    run_at_ms: i64,
    idempotency_key: Option<String>,
    fingerprint: &'s str,
    callback_url: Option<&'s str>,
    callback_for: Option<&'s str>,
    capture: bool,
    lane: Option<&'s str>,
    cron: Option<&'s str>,
    alert_for: Option<&'s str>,
    wake_call: bool,
    /// Report to `callback_url` only if the task fails (a cron run with `report: failures`).
    report_failures_only: bool,
    /// A watch's settings, as JSON.
    watch: Option<String>,
}

/// Callback task ids are derived, so queueing the same callback twice is harmless. A task's
/// generation differs every time it finishes (only a redrive can follow a finish, and it
/// moves the generation on), so each finish gets its own callback.
fn callback_id(task_id: &str, generation: u64) -> String {
    derived_id(&format!("callback/{task_id}/{generation}"))
}

fn derived_id(seed: &str) -> String {
    let digest = Sha256::digest(seed);
    format!("task_{}", hex::encode(&digest[..10]))
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// No run has finished yet.
    Pending,
    /// The last run that finished succeeded.
    Healthy,
    /// A run failed, or one could not be started; see `health_reason`.
    Failing,
}

/// Header values and the body are never echoed back, as for tasks.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct CronView {
    pub name: String,
    pub schedule: String,
    pub method: Method,
    pub url: String,
    pub header_names: Vec<String>,
    pub body_bytes: usize,
    pub max_attempts: u32,
    pub alert_url: Option<String>,
    /// Where each run reports when it finishes (a URL or "inbox"), and which runs: always,
    /// or failures only.
    pub callback_url: Option<String>,
    pub report: Report,
    /// Whether each run keeps the destination's response.
    pub capture: bool,
    /// None once the cron has stopped (the trial ended).
    pub next_run_ms: Option<i64>,
    pub health: Health,
    /// Why it is failing: run_failed, budget_exhausted, task_limit or trial_expired.
    pub health_reason: Option<String>,
    pub last_run_task_id: Option<String>,
    pub last_run_ms: Option<i64>,
    pub last_success_ms: Option<i64>,
    pub failing_since_ms: Option<i64>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct CronList {
    pub crons: Vec<CronView>,
}

#[derive(Deserialize)]
struct CronRow {
    name: String,
    schedule: String,
    method: String,
    url: String,
    headers: String,
    body: Option<String>,
    max_attempts: u32,
    alert_url: Option<String>,
    callback_url: Option<String>,
    capture: i64,
    report: String,
    next_run_ms: Option<i64>,
    health: String,
    health_reason: Option<String>,
    last_run_task_id: Option<String>,
    last_run_ms: Option<i64>,
    last_success_ms: Option<i64>,
    failing_since_ms: Option<i64>,
    /// Slot of the run (or missed run) that last set `health`; older news is ignored.
    health_run_at_ms: Option<i64>,
    created_ms: i64,
    updated_ms: i64,
}

const CRON_COLUMNS: &str = "name, schedule, method, url, headers, body, max_attempts, \
    alert_url, callback_url, capture, report, next_run_ms, health, health_reason, last_run_task_id, last_run_ms, \
    last_success_ms, failing_since_ms, health_run_at_ms, created_ms, updated_ms";

impl CronRow {
    fn headers(&self) -> Result<Vec<(String, String)>, DbError> {
        serde_json::from_str(&self.headers).map_err(|e| DbError(e.to_string()))
    }

    fn view(&self) -> Result<CronView, DbError> {
        Ok(CronView {
            name: self.name.clone(),
            schedule: self.schedule.clone(),
            method: parse(&self.method)?,
            url: self.url.clone(),
            header_names: self.headers()?.into_iter().map(|(k, _)| k).collect(),
            body_bytes: self.body.as_ref().map_or(0, String::len),
            max_attempts: self.max_attempts,
            alert_url: self.alert_url.clone(),
            callback_url: self.callback_url.clone(),
            report: parse(&self.report)?,
            capture: self.capture != 0,
            next_run_ms: self.next_run_ms,
            health: match self.health.as_str() {
                "healthy" => Health::Healthy,
                "failing" => Health::Failing,
                _ => Health::Pending,
            },
            health_reason: self.health_reason.clone(),
            last_run_task_id: self.last_run_task_id.clone(),
            last_run_ms: self.last_run_ms,
            last_success_ms: self.last_success_ms,
            failing_since_ms: self.failing_since_ms,
            created_ms: self.created_ms,
            updated_ms: self.updated_ms,
        })
    }
}

fn no_cron() -> ApiError {
    ApiError::not_found("cron_not_found", "no cron with that name in this project")
}

// ---------------------------------------------------------------------------------------

pub struct Ledger<'a, D: Db> {
    db: &'a D,
    /// Whether the API can open sealed secrets. Without its key, work that needs a secret
    /// fails before it is attempted rather than spending an attempt it can't make.
    secrets_available: bool,
}

impl<'a, D: Db> Ledger<'a, D> {
    pub fn open(db: &'a D) -> Result<Self, DbError> {
        for statement in SCHEMA {
            db.exec(statement, vec![])?;
        }
        let applied: Vec<MetaRow> =
            db.query("SELECT value FROM meta WHERE key = 'schema'", vec![])?;
        let applied = applied.first().map_or(0, |m| m.value as usize);
        if applied < MIGRATIONS.len() {
            for statement in MIGRATIONS[applied..].iter().copied().flatten() {
                // Storage errors do not roll back earlier statements, so a migration that
                // stopped halfway is resumed: a column it already added is not an error.
                match db.exec(statement, vec![]) {
                    Err(e) if e.0.contains("duplicate column name") => {}
                    other => other?,
                }
            }
            db.exec(
                "INSERT INTO meta (key, value) VALUES ('schema', ?) \
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                vec![(MIGRATIONS.len() as i64).into()],
            )?;
        }
        Ok(Self {
            db,
            secrets_available: true,
        })
    }

    /// The ledger for an API that has no key to open secrets with.
    pub fn without_secrets(self) -> Self {
        Self {
            secrets_available: false,
            ..self
        }
    }

    // -- project -------------------------------------------------------------------------

    fn project_row(&self) -> Result<Option<ProjectRow>, DbError> {
        Ok(self
            .db
            .query(
                "SELECT id, created_ms, expires_ms, attempt_budget, attempts_used, signing_secret, \
                 EXISTS (SELECT 1 FROM payments) AS paid, inbox_last_seq, inbox_acked_seq, \
                 inbox_dropped, (SELECT COUNT(*) FROM inbox WHERE seq > p.inbox_acked_seq) \
                 AS inbox_unread FROM project p LIMIT 1",
                vec![],
            )?
            .pop())
    }

    pub fn project(&self) -> Result<ProjectView, ApiError> {
        let mut view = self.project_row()?.ok_or_else(no_project)?.view();
        view.wake = self.wake_row()?.map(|w| w.view()).transpose()?;
        Ok(view)
    }

    /// Creates the project with a fresh trial, or returns the existing one unchanged.
    /// `signing_secret` is only used on creation.
    pub fn create_project(
        &self,
        project_id: &str,
        signing_secret: &str,
        now: i64,
    ) -> Result<(ProjectView, bool), ApiError> {
        if self.project_row()?.is_some() {
            return Ok((self.project()?, false));
        }
        self.db.exec(
            "INSERT INTO project (id, created_ms, expires_ms, attempt_budget, attempts_used, \
             signing_secret) VALUES (?, ?, ?, ?, 0, ?)",
            vec![
                project_id.into(),
                now.into(),
                (now + TRIAL_DURATION_MS).into(),
                TRIAL_ATTEMPTS.into(),
                signing_secret.into(),
            ],
        )?;
        Ok((self.project()?, true))
    }

    /// Adds `attempts` bought with payment `payment_id`, and makes the whole balance valid for
    /// [`CREDIT_VALIDITY_MS`] from now. Attempts left from an earlier purchase or the trial carry
    /// over unless they have already expired. Crons stopped by expiry start again. Returns the
    /// project and whether the payment was new; a payment already applied changes nothing.
    pub fn add_credit(
        &self,
        payment_id: &str,
        attempts: i64,
        now: i64,
    ) -> Result<(ProjectView, bool), ApiError> {
        if payment_id.is_empty()
            || payment_id.len() > 255
            || !payment_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(ApiError {
                field: Some("payment_id"),
                ..ApiError::bad_request("payment_id is not a valid payment id")
            });
        }
        if !(1..=MAX_CREDIT_ATTEMPTS).contains(&attempts) {
            return Err(ApiError {
                field: Some("attempts"),
                ..ApiError::bad_request(format!("attempts must be 1-{MAX_CREDIT_ATTEMPTS}"))
            });
        }
        let project = self.project_row()?.ok_or_else(no_project)?;
        let applied: Vec<Counts> = self.db.query(
            "SELECT COUNT(*) AS total, 0 AS waiting FROM payments WHERE id = ?",
            vec![payment_id.into()],
        )?;
        if applied.first().map_or(0, |c| c.total) > 0 {
            return Ok((self.project()?, false));
        }
        let left = if now < project.expires_ms {
            (project.attempt_budget - project.attempts_used).max(0)
        } else {
            0
        };
        // Credit before recording the payment: if recording fails, Stripe's retry credits again,
        // which costs RunLater a little; the other order could leave a paid pack never credited.
        self.db.exec(
            "UPDATE project SET attempt_budget = ?, expires_ms = ?",
            vec![
                (project.attempts_used + left + attempts).into(),
                project.expires_ms.max(now + CREDIT_VALIDITY_MS).into(),
            ],
        )?;
        self.db.exec(
            "INSERT INTO payments (id, attempts, created_ms) VALUES (?, ?, ?)",
            vec![payment_id.into(), attempts.into(), now.into()],
        )?;
        let stopped: Vec<CronRow> = self.db.query(
            &format!("SELECT {CRON_COLUMNS} FROM crons WHERE next_run_ms IS NULL"),
            vec![],
        )?;
        for cron in stopped {
            // Schedules were validated when stored; one that no longer parses stays stopped
            // rather than failing a payment that has already been applied.
            let Ok(schedule) = Schedule::parse(&cron.schedule) else {
                continue;
            };
            self.db.exec(
                "UPDATE crons SET next_run_ms = ?, updated_ms = ? WHERE name = ?",
                vec![
                    schedule.next_after(now).into(),
                    now.into(),
                    cron.name.as_str().into(),
                ],
            )?;
        }
        Ok((self.project()?, true))
    }

    /// Admission for new work: the trial must be live, every waiting task (and the callback
    /// it will send) must still be able to get at least one attempt, and new rows must fit
    /// under the retained-task cap. `rows` and `attempts` are what the new work needs.
    fn admit(
        &self,
        project: &ProjectRow,
        rows: i64,
        attempts: i64,
        now: i64,
    ) -> Result<(), ApiError> {
        if now >= project.expires_ms {
            return Err(ApiError::new(
                403,
                "trial_expired",
                "this project's attempts have expired",
            ));
        }
        let counts: Vec<Counts> = self.db.query(
            &format!(
                "SELECT COUNT(*) + COALESCE(SUM(status IN ('scheduled', 'retrying', 'delivering') \
                 AND {OWES_CALLBACK}), 0) AS total, \
                 COALESCE(SUM(CASE WHEN status IN ('scheduled', 'retrying', 'delivering') \
                 THEN 1 + {OWES_CALLBACK} ELSE 0 END), 0) AS waiting \
                 FROM tasks"
            ),
            vec![],
        )?;
        let (total, waiting) = counts.first().map_or((0, 0), |c| (c.total, c.waiting));
        let cap = project.max_retained_tasks();
        if rows > 0 && total + rows > cap {
            return Err(ApiError::new(
                429,
                "task_limit",
                format!(
                    "this project already holds {cap} tasks; \
                     finished tasks are removed 30 days after they finish"
                ),
            ));
        }
        if waiting + attempts > project.attempt_budget - project.attempts_used {
            return Err(ApiError::new(
                402,
                "budget_exhausted",
                "not enough delivery attempts left in this project",
            ));
        }
        Ok(())
    }

    // -- tasks: commands ------------------------------------------------------------------

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

        let needs = 1 + i64::from(matches!(spec.callback, Some(Callback::Url(_))));
        self.admit(&project, needs, needs, now)?;
        if spec.run_at_ms >= project.expires_ms {
            return Err(ApiError {
                field: Some(when_field),
                ..ApiError::bad_request(format!("{when_field} is after this project's trial ends"))
            });
        }

        self.insert(
            NewTask {
                id: task_id,
                task: Task::new(spec.run_at_ms, spec.max_attempts),
                method: spec.method,
                url: spec.destination.as_str(),
                headers: &spec.headers,
                body: spec.body,
                run_at_ms: spec.run_at_ms,
                idempotency_key: spec.idempotency_key,
                fingerprint: &spec.fingerprint,
                callback_url: spec.callback.as_ref().map(|c| c.as_str()),
                callback_for: None,
                capture: spec.capture,
                lane: spec.lane.as_deref(),
                cron: None,
                alert_for: None,
                wake_call: false,
                report_failures_only: false,
                watch: None,
            },
            now,
        )?;
        Ok((self.task(task_id)?, true))
    }

    /// Starts a watch: a task whose attempts poll `url` until `until` matches. Returns the
    /// task and whether it was created (false for an idempotent replay).
    pub fn watch(
        &self,
        request: WatchRequest,
        task_id: &str,
        now: i64,
    ) -> Result<(TaskView, bool), ApiError> {
        let project = self.project_row()?.ok_or_else(no_project)?;
        let spec = request.validate(now).map_err(ApiError::invalid_watch)?;
        self.check_secrets(&spec.request.headers, spec.request.destination.host())?;
        let request = spec.request;
        if let Some(existing) =
            self.replay(request.idempotency_key.as_deref(), &request.fingerprint)?
        {
            return Ok((existing, false));
        }
        if spec.watch.deadline_ms >= project.expires_ms {
            return Err(ApiError {
                field: Some("deadline"),
                ..ApiError::bad_request("deadline is after this project's trial ends")
            });
        }
        let needs = 1 + i64::from(matches!(request.callback, Some(Callback::Url(_))));
        self.admit(&project, needs, needs, now)?;
        let watch = serde_json::to_string(&spec.watch).map_err(|e| DbError(e.to_string()))?;
        self.insert(
            NewTask {
                id: task_id,
                task: Task::new(request.run_at_ms, request.max_attempts),
                method: request.method,
                url: request.destination.as_str(),
                headers: &request.headers,
                body: request.body,
                run_at_ms: request.run_at_ms,
                idempotency_key: request.idempotency_key,
                fingerprint: &request.fingerprint,
                callback_url: request.callback.as_ref().map(|c| c.as_str()),
                callback_for: None,
                capture: false,
                lane: None,
                cron: None,
                alert_for: None,
                wake_call: false,
                report_failures_only: false,
                watch: Some(watch),
            },
            now,
        )?;
        Ok((self.task(task_id)?, true))
    }

    /// The task an idempotency key already made, if any; a different request under the same
    /// key is a conflict.
    fn replay(&self, key: Option<&str>, fingerprint: &str) -> Result<Option<TaskView>, ApiError> {
        let Some(key) = key else {
            return Ok(None);
        };
        let existing: Vec<KeyRow> = self.db.query(
            "SELECT id, fingerprint FROM tasks WHERE idempotency_key = ?",
            vec![key.into()],
        )?;
        let Some(existing) = existing.first() else {
            return Ok(None);
        };
        if existing.fingerprint != fingerprint {
            return Err(ApiError::new(
                409,
                "idempotency_conflict",
                "idempotency_key was already used for a different request",
            ));
        }
        Ok(Some(self.task(&existing.id)?))
    }

    pub fn cancel(&self, task_id: &str, now: i64) -> Result<TaskView, ApiError> {
        let row = self.task_row(task_id)?.ok_or_else(no_task)?;
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
        let row = self.task_row(task_id)?.ok_or_else(no_task)?;
        if row.watch.is_some() {
            return Err(ApiError::new(
                409,
                "watch_not_redrivable",
                "a watch cannot be redriven; start a new one",
            ));
        }
        if row.wake_call != 0 {
            return Err(ApiError::new(
                409,
                "wake_not_redrivable",
                "a wake cannot be redriven; the next inbox event sends a new one",
            ));
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
        let callbacks = i64::from(row.callback_url.as_deref().is_some_and(|u| u != INBOX));
        self.admit(&project, callbacks, 1 + callbacks, now)?;
        self.save_row(&row, &task, now)?;
        self.db.exec(
            "UPDATE tasks SET failure = NULL WHERE id = ?",
            vec![task_id.into()],
        )?;
        self.task(task_id)
    }

    // -- tasks: queries -------------------------------------------------------------------

    fn task_row(&self, task_id: &str) -> Result<Option<TaskRow>, DbError> {
        Ok(self
            .db
            .query(
                &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?"),
                vec![task_id.into()],
            )?
            .pop())
    }

    /// One task with its attempt history.
    pub fn task(&self, task_id: &str) -> Result<TaskView, ApiError> {
        let row = self.task_row(task_id)?.ok_or_else(no_task)?;
        Ok(self.full_view(&row)?)
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

    // -- delivery -------------------------------------------------------------------------

    /// Due tasks to put on the queue. Marks them published so the next alarm does not
    /// republish them until [`REPUBLISH_MS`] has passed without a claim.
    pub fn publish_due(&self, project_id: &str, now: i64) -> Result<Vec<Dispatch>, DbError> {
        let due: Vec<DueRow> = self.db.query(
            &format!(
                "SELECT id, generation FROM tasks t \
                 WHERE status IN ('scheduled', 'retrying') AND due_ms <= ? \
                 AND (published_ms IS NULL OR published_ms <= ?) AND {LANE_CLEAR} \
                 ORDER BY due_ms LIMIT ?"
            ),
            vec![
                now.into(),
                (now - REPUBLISH_MS).into(),
                (PUBLISH_BATCH as i64).into(),
            ],
        )?;
        for d in &due {
            self.db.exec(
                "UPDATE tasks SET published_ms = ? WHERE id = ?",
                vec![now.into(), d.id.as_str().into()],
            )?;
        }
        Ok(due
            .into_iter()
            .map(|d| Dispatch {
                project_id: project_id.into(),
                task_id: d.id,
                generation: d.generation,
            })
            .collect())
    }

    /// Starts an attempt for a queue message, charging one attempt to the trial.
    pub fn claim(&self, task_id: &str, generation: u64, now: i64) -> Result<Claimed, ApiError> {
        let Some(project) = self.project_row()? else {
            return Ok(Claimed::Skip("project_missing"));
        };
        let Some(row) = self.task_row(task_id)? else {
            return Ok(Claimed::Skip("task_missing"));
        };
        let mut task = row.task()?;

        // A callback reports on work already done, so it gets one try even when the trial
        // has run out, but no retries.
        let mut last_try =
            (row.callback_for.is_some() || row.alert_for.is_some()) && task.run_attempts == 0;
        // A wake was never admitted, so it may only spend attempts no waiting task was promised.
        // With nothing else waiting it gets one more try even past the budget; otherwise it
        // stands down (canceled, not failed: the wake itself is fine). And a wake due after the
        // agent read everything has nothing to say.
        if row.wake_call != 0 && task.generation == generation && task.is_due(now) {
            let left = if now < project.expires_ms {
                project.attempt_budget - project.attempts_used
            } else {
                0
            };
            let reserved = self.reserved_attempts()?;
            let skip = if project.inbox_unread == 0 {
                Some("inbox_read")
            } else if left - reserved >= 1 {
                None
            } else if reserved == 0 {
                last_try = true;
                None
            } else {
                Some("budget_reserved")
            };
            if let Some(why) = skip {
                if task.cancel(now).is_ok() {
                    self.save_row(&row, &task, now)?;
                }
                return Ok(Claimed::Skip(why));
            }
        }
        if !project.can_attempt(now) && last_try {
            task.max_attempts = task.run_attempts + 1;
        } else if !project.can_attempt(now) {
            if task.generation == generation && task.is_due(now) && task.abandon(now) {
                if row.watch.is_some() {
                    let state = WatchState {
                        reason: Some("budget_exhausted".into()),
                        ..row.watch_state()?
                    };
                    self.store_watch(task_id, row.watch_errors, &state)?;
                }
                self.save_row(&row, &task, now)?;
                return Ok(Claimed::Skip("budget_exhausted"));
            }
            return Ok(Claimed::Skip("stale"));
        }

        if row.lane.is_some()
            && task.generation == generation
            && task.is_due(now)
            && !self.lane_clear(task_id)?
        {
            // Published while it was the lane's head, but something else got there first.
            // It is published again once it is next.
            self.db.exec(
                "UPDATE tasks SET published_ms = NULL WHERE id = ?",
                vec![task_id.into()],
            )?;
            return Ok(Claimed::Skip("lane_busy"));
        }

        // Secrets are checked again now: one may have been deleted, or narrowed to other hosts,
        // since the task was scheduled. Either way it cannot be sent, so it fails at once.
        let secrets = match self.sealed_for(&row)? {
            Ok(secrets) => secrets,
            Err(why) => {
                if task.generation == generation && task.is_due(now) && task.abandon(now) {
                    self.db.exec(
                        "UPDATE tasks SET failure = ? WHERE id = ?",
                        vec![why.into(), task_id.into()],
                    )?;
                    if row.watch.is_some() {
                        let state = WatchState {
                            reason: Some(why.into()),
                            ..row.watch_state()?
                        };
                        self.store_watch(task_id, row.watch_errors, &state)?;
                    }
                    self.save_row(&row, &task, now)?;
                    return Ok(Claimed::Skip(why));
                }
                return Ok(Claimed::Skip("stale"));
            }
        };
        let attempt: Attempt = match task.claim(generation, now) {
            Ok(a) => a,
            Err(ClaimError::Stale) => return Ok(Claimed::Skip("stale")),
            Err(ClaimError::NotDue { .. }) => return Ok(Claimed::Skip("not_due")),
        };
        self.db.exec(
            "UPDATE project SET attempts_used = attempts_used + 1",
            vec![],
        )?;
        self.db.exec(
            "INSERT INTO attempts (task_id, number, started_ms) VALUES (?, ?, ?)",
            vec![task_id.into(), attempt.number.into(), now.into()],
        )?;
        self.save_row(&row, &task, now)?;
        // A wake says how things stand when it is sent, and covers every event so far.
        let body = if row.wake_call != 0 {
            self.db.exec(
                "UPDATE wake SET covered_seq = ?, last_wake_ms = ?",
                vec![project.inbox_last_seq.into(), now.into()],
            )?;
            Some(wake_payload(&project.id, project.inbox_unread))
        } else {
            row.body.clone()
        };
        Ok(Claimed::Deliver(Box::new(Delivery {
            task_id: task_id.into(),
            attempt: attempt.number,
            fence: attempt.fence,
            method: parse(&row.method)?,
            url: row.url.clone(),
            headers: row.headers()?,
            body,
            signing_secret: project.signing_secret,
            read_body: row.watch.is_some() || row.capture != 0,
            capture: row.capture != 0,
            secrets,
        })))
    }

    /// Records an attempt's result. `None` if the attempt no longer holds the task.
    pub fn finish(
        &self,
        task_id: &str,
        fence: u64,
        outcome: Outcome,
        now: i64,
        jitter: f64,
    ) -> Result<Option<Status>, DbError> {
        self.finish_response(task_id, fence, outcome, None, now, jitter)
    }

    /// [`Self::finish`], with the response when the consumer read it (a watch's poll, or a
    /// task with `capture`).
    pub fn finish_response(
        &self,
        task_id: &str,
        fence: u64,
        outcome: Outcome,
        read: Option<ResponseRead<'_>>,
        now: i64,
        jitter: f64,
    ) -> Result<Option<Status>, DbError> {
        let Some(row) = self.task_row(task_id)? else {
            return Ok(None);
        };
        if row.watch.is_some() {
            return self.finish_poll(&row, fence, outcome, read.and_then(|r| r.body), now);
        }
        let mut task = row.task()?;
        let Ok(status) = task.finish(fence, outcome, now, jitter) else {
            return Ok(None);
        };
        self.record_outcome(task_id, fence, &outcome, now)?;
        // Before save_row, so a callback or inbox event for this finish carries the response.
        // Any HTTP answer replaces the last capture, even one whose body could not be read:
        // an older response must never stand in for a newer one.
        if let (true, Outcome::Http { status, .. }) = (row.capture != 0, &outcome) {
            self.store_response(task_id, fence, *status, read)?;
        }
        self.save_row(&row, &task, now)?;
        Ok(Some(status))
    }

    /// Keeps a response for `capture`, replacing the one from an earlier attempt.
    fn store_response(
        &self,
        task_id: &str,
        attempt: u64,
        status: u16,
        read: Option<ResponseRead<'_>>,
    ) -> Result<(), DbError> {
        let mut headers = BTreeMap::new();
        let mut length = None;
        let mut encoded = false;
        for (name, value) in read.map_or(&[][..], |r| r.headers) {
            let name = name.to_ascii_lowercase();
            match name.as_str() {
                "content-length" => length = value.trim().parse::<u64>().ok(),
                "content-encoding" => encoded = true,
                _ if CAPTURED_HEADERS.contains(&name.as_str()) => {
                    headers.insert(name, value.clone());
                }
                _ => {}
            }
        }
        // Content-Length of an encoded body is the compressed size, not the body's.
        let body_bytes = length.filter(|_| !encoded);
        let read_body = read.and_then(|r| r.body);
        let body = read_body.map(|b| excerpt(b.text, CAPTURE_BODY_BYTES));
        let truncated = match (read_body, &body) {
            (Some(b), Some(kept)) => {
                b.truncated
                    || kept.len() < b.text.len()
                    || body_bytes.is_some_and(|n| n > kept.len() as u64)
            }
            _ => true,
        };
        let response = CapturedResponse {
            attempt: attempt as u32,
            status,
            headers,
            body,
            body_bytes,
            truncated,
        };
        let response = serde_json::to_string(&response).map_err(|e| DbError(e.to_string()))?;
        self.db.exec(
            "UPDATE tasks SET response = ? WHERE id = ?",
            vec![response.into(), task_id.into()],
        )
    }

    /// Settles attempts whose consumer vanished. Returns how many were recovered.
    pub fn recover(&self, now: i64, mut jitter: impl FnMut() -> f64) -> Result<usize, DbError> {
        let rows: Vec<TaskRow> = self.db.query(
            &format!(
                "SELECT {TASK_COLUMNS} FROM tasks \
                 WHERE status = 'delivering' AND lease_until_ms <= ?"
            ),
            vec![now.into()],
        )?;
        for row in &rows {
            let mut task = row.task()?;
            let fence = task.fence;
            if row.watch.is_some() {
                self.finish_poll(row, fence, Outcome::LeaseExpired, None, now)?;
                continue;
            }
            if task.expire_lease(now, jitter()).is_some() {
                self.record_outcome(&row.id, fence, &Outcome::LeaseExpired, now)?;
                self.save_row(row, &task, now)?;
            }
        }
        Ok(rows.len())
    }

    /// Deletes tasks whose retention period has passed, captured responses a week after
    /// their task finished, and old inbox events. Returns how many tasks.
    pub fn purge(&self, now: i64) -> Result<usize, DbError> {
        self.db.exec(
            "UPDATE tasks SET response = NULL WHERE response IS NOT NULL \
             AND terminal_ms IS NOT NULL AND terminal_ms <= ?",
            vec![(now - CAPTURE_RETENTION_MS).into()],
        )?;
        self.purge_inbox(now)?;
        self.expire_inbox_responses(now)?;
        let cutoff = now - RETENTION_MS;
        let expired: Vec<IdRow> = self.db.query(
            "SELECT id FROM tasks WHERE terminal_ms IS NOT NULL AND terminal_ms <= ?",
            vec![cutoff.into()],
        )?;
        for row in &expired {
            self.db.exec(
                "DELETE FROM attempts WHERE task_id = ?",
                vec![row.id.as_str().into()],
            )?;
            self.db.exec(
                "DELETE FROM tasks WHERE id = ?",
                vec![row.id.as_str().into()],
            )?;
        }
        Ok(expired.len())
    }

    /// When the alarm should next run, if there is anything left to do.
    pub fn next_wake(&self) -> Result<Option<i64>, DbError> {
        let rows: Vec<Wake> = self.db.query(
            &format!(
                "SELECT MIN(t) AS t FROM ( \
                 SELECT CASE WHEN published_ms IS NOT NULL AND published_ms + {REPUBLISH_MS} > due_ms \
                        THEN published_ms + {REPUBLISH_MS} ELSE due_ms END AS t \
                   FROM tasks t WHERE status IN ('scheduled', 'retrying') AND {LANE_CLEAR} \
                 UNION ALL SELECT lease_until_ms FROM tasks WHERE status = 'delivering' \
                 UNION ALL SELECT terminal_ms + {RETENTION_MS} FROM tasks WHERE terminal_ms IS NOT NULL \
                 UNION ALL SELECT MIN(next_run_ms, COALESCE((SELECT expires_ms FROM project), next_run_ms)) \
                   FROM crons WHERE next_run_ms IS NOT NULL)"
            ),
            vec![],
        )?;
        // A second query: Durable Object SQLite allows at most five terms in a compound SELECT.
        let deletions: Vec<Wake> = self.db.query(
            &format!(
                "SELECT MIN(t) AS t FROM ( \
                 SELECT terminal_ms + {CAPTURE_RETENTION_MS} AS t FROM tasks \
                   WHERE response IS NOT NULL AND terminal_ms IS NOT NULL \
                 UNION ALL SELECT MIN(at_ms) + {INBOX_ACKED_RETENTION_MS} FROM inbox \
                   WHERE seq <= (SELECT inbox_acked_seq FROM project) \
                 UNION ALL SELECT MIN(at_ms) + {RETENTION_MS} FROM inbox \
                   WHERE seq > (SELECT inbox_acked_seq FROM project) \
                 UNION ALL SELECT MIN(response_until_ms) FROM inbox)"
            ),
            vec![],
        )?;
        Ok(rows.iter().chain(&deletions).filter_map(|w| w.t).min())
    }

    // -- crons ----------------------------------------------------------------------------

    fn cron_row(&self, name: &str) -> Result<Option<CronRow>, DbError> {
        Ok(self
            .db
            .query(
                &format!("SELECT {CRON_COLUMNS} FROM crons WHERE name = ?"),
                vec![name.into()],
            )?
            .pop())
    }

    /// Creates the cron, or replaces the definition of the one with this name (its health
    /// and history are kept). Returns whether it was created.
    pub fn put_cron(&self, request: CronRequest, now: i64) -> Result<(CronView, bool), ApiError> {
        let project = self.project_row()?.ok_or_else(no_project)?;
        if now >= project.expires_ms {
            return Err(ApiError::new(
                403,
                "trial_expired",
                "this project's attempts have expired",
            ));
        }
        let spec = request.validate(now).map_err(ApiError::invalid_cron)?;
        self.check_secrets(&spec.request.headers, spec.request.destination.host())?;
        let existing = self.cron_row(&spec.name)?;
        if existing.is_none() {
            let count: Vec<Counts> = self
                .db
                .query("SELECT COUNT(*) AS total, 0 AS waiting FROM crons", vec![])?;
            if count.first().map_or(0, |c| c.total) >= MAX_CRONS {
                return Err(ApiError::new(
                    429,
                    "cron_limit",
                    format!("this project already has {MAX_CRONS} crons; delete one first"),
                ));
            }
        }
        let request = &spec.request;
        let headers =
            serde_json::to_string(&request.headers).map_err(|e| DbError(e.to_string()))?;
        self.db.exec(
            "INSERT INTO crons (name, schedule, method, url, headers, body, max_attempts, \
             alert_url, callback_url, capture, report, next_run_ms, health, created_ms, \
             updated_ms) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?) \
             ON CONFLICT (name) DO UPDATE SET schedule = excluded.schedule, \
             method = excluded.method, url = excluded.url, headers = excluded.headers, \
             body = excluded.body, max_attempts = excluded.max_attempts, \
             alert_url = excluded.alert_url, callback_url = excluded.callback_url, \
             capture = excluded.capture, report = excluded.report, \
             next_run_ms = excluded.next_run_ms, updated_ms = excluded.updated_ms",
            vec![
                spec.name.as_str().into(),
                spec.schedule_text.as_str().into(),
                request.method.as_str().into(),
                request.destination.as_str().into(),
                headers.into(),
                request.body.clone().into(),
                request.max_attempts.into(),
                spec.alert.as_ref().map(|a| a.as_str()).into(),
                request.callback.as_ref().map(|c| c.as_str()).into(),
                request.capture.into(),
                spec.report.as_str().into(),
                spec.schedule.next_after(now).into(),
                now.into(),
                now.into(),
            ],
        )?;
        Ok((self.cron(&spec.name)?, existing.is_none()))
    }

    pub fn cron(&self, name: &str) -> Result<CronView, ApiError> {
        Ok(self.cron_row(name)?.ok_or_else(no_cron)?.view()?)
    }

    pub fn crons(&self) -> Result<CronList, ApiError> {
        self.project_row()?.ok_or_else(no_project)?;
        let rows: Vec<CronRow> = self.db.query(
            &format!("SELECT {CRON_COLUMNS} FROM crons ORDER BY name"),
            vec![],
        )?;
        let crons = rows.iter().map(CronRow::view).collect::<Result<_, _>>()?;
        Ok(CronList { crons })
    }

    /// Stops the cron. Runs already started finish normally.
    pub fn delete_cron(&self, name: &str) -> Result<CronView, ApiError> {
        let view = self.cron(name)?;
        self.db
            .exec("DELETE FROM crons WHERE name = ?", vec![name.into()])?;
        Ok(view)
    }

    /// Starts a run for every cron that is due. A run that cannot be admitted, or a trial
    /// that has ended, marks the cron failing (and alerts) instead. Missed slots are not
    /// made up: a cron runs at most once per call. Returns how many runs started.
    pub fn fire_crons(&self, now: i64) -> Result<usize, DbError> {
        let Some(project) = self.project_row()? else {
            return Ok(0);
        };
        // Once the trial has ended, every cron that is still scheduled stops now.
        let until = if now >= project.expires_ms {
            MAX_SAFE_INTEGER
        } else {
            now
        };
        let due: Vec<CronRow> = self.db.query(
            &format!(
                "SELECT {CRON_COLUMNS} FROM crons WHERE next_run_ms IS NOT NULL AND next_run_ms <= ?"
            ),
            vec![until.into()],
        )?;
        let mut started = 0;
        for cron in &due {
            if now >= project.expires_ms {
                self.db.exec(
                    "UPDATE crons SET next_run_ms = NULL, updated_ms = ? WHERE name = ?",
                    vec![now.into(), cron.name.as_str().into()],
                )?;
                self.cron_failing(&cron.name, "trial_expired", None, now, now)?;
                continue;
            }
            let schedule = Schedule::parse(&cron.schedule).map_err(|e| DbError(e.to_string()))?;
            let slot = cron.next_run_ms.unwrap_or(now);
            self.db.exec(
                "UPDATE crons SET next_run_ms = ?, updated_ms = ? WHERE name = ?",
                vec![
                    schedule.next_after(now).into(),
                    now.into(),
                    cron.name.as_str().into(),
                ],
            )?;
            // A run reporting to a URL needs a second attempt and row for its callback task.
            let needs = 1 + i64::from(cron.callback_url.as_deref().is_some_and(|u| u != INBOX));
            if let Err(e) = self.admit(&project, needs, needs, now) {
                if e.status >= 500 {
                    return Err(DbError(e.message));
                }
                self.cron_failing(&cron.name, e.code, None, slot, now)?;
                continue;
            }
            let id = derived_id(&format!("cron/{}/{slot}", cron.name));
            self.insert(
                NewTask {
                    id: &id,
                    task: Task::new(now, cron.max_attempts),
                    method: parse(&cron.method)?,
                    url: &cron.url,
                    headers: &cron.headers()?,
                    body: cron.body.clone(),
                    run_at_ms: slot,
                    idempotency_key: None,
                    fingerprint: "cron",
                    callback_url: cron.callback_url.as_deref(),
                    callback_for: None,
                    capture: cron.capture != 0,
                    lane: None,
                    cron: Some(&cron.name),
                    alert_for: None,
                    wake_call: false,
                    report_failures_only: cron.report == Report::Failures.as_str(),
                    watch: None,
                },
                now,
            )?;
            self.db.exec(
                "UPDATE crons SET last_run_task_id = ?, last_run_ms = ? WHERE name = ?",
                vec![id.as_str().into(), now.into(), cron.name.as_str().into()],
            )?;
            started += 1;
        }
        Ok(started)
    }

    /// A run reached a final state: update its cron's health. Canceled runs say nothing,
    /// nor do runs of a cron that has since been deleted (even if re-created by name), nor
    /// runs older than the one that last set the cron's health.
    fn run_finished(
        &self,
        name: &str,
        run: &TaskRow,
        status: Status,
        now: i64,
    ) -> Result<(), DbError> {
        let Some(cron) = self.cron_row(name)? else {
            return Ok(());
        };
        if run.created_ms <= cron.created_ms {
            return Ok(());
        }
        match status {
            Status::Succeeded => {
                if cron.health_run_at_ms.is_some_and(|t| t > run.run_at_ms) {
                    return Ok(());
                }
                self.db.exec(
                    "UPDATE crons SET health = 'healthy', health_reason = NULL, \
                     failing_since_ms = NULL, last_success_ms = ?, health_run_at_ms = ?, \
                     updated_ms = ? WHERE name = ?",
                    vec![now.into(), run.run_at_ms.into(), now.into(), name.into()],
                )?;
                if cron.health == "failing" {
                    self.queue_alert(name, "cron.recovered", None, Some(&run.id), now)?;
                }
                Ok(())
            }
            Status::Failed => {
                self.cron_failing(name, "run_failed", Some(&run.id), run.run_at_ms, now)
            }
            _ => Ok(()),
        }
    }

    /// Marks the cron failing because of the run (or missed run) for slot `at`; alerts only
    /// when it was not failing already.
    fn cron_failing(
        &self,
        name: &str,
        reason: &str,
        task_id: Option<&str>,
        at: i64,
        now: i64,
    ) -> Result<(), DbError> {
        let Some(cron) = self.cron_row(name)? else {
            return Ok(());
        };
        if cron.health_run_at_ms.is_some_and(|t| t > at) {
            return Ok(());
        }
        if cron.health == "failing" {
            self.db.exec(
                "UPDATE crons SET health_reason = ?, health_run_at_ms = ?, updated_ms = ? \
                 WHERE name = ?",
                vec![reason.into(), at.into(), now.into(), name.into()],
            )?;
            return Ok(());
        }
        self.db.exec(
            "UPDATE crons SET health = 'failing', health_reason = ?, failing_since_ms = ?, \
             health_run_at_ms = ?, updated_ms = ? WHERE name = ?",
            vec![
                reason.into(),
                now.into(),
                at.into(),
                now.into(),
                name.into(),
            ],
        )?;
        self.queue_alert(name, "cron.failing", Some(reason), task_id, now)
    }

    /// Alerts are ordinary tasks, like callbacks: signed, retried, one try past the trial.
    fn queue_alert(
        &self,
        name: &str,
        event: &str,
        reason: Option<&str>,
        task_id: Option<&str>,
        now: i64,
    ) -> Result<(), DbError> {
        let Some(cron) = self.cron_row(name)? else {
            return Ok(());
        };
        let Some(url) = &cron.alert_url else {
            return Ok(());
        };
        let report = serde_json::json!({
            "event": event,
            "reason": reason,
            "task_id": task_id,
            "cron": cron.view()?,
        });
        if url == INBOX {
            let seq = self.next_inbox_seq()?;
            self.add_to_inbox(seq, &report, now)?;
            return self.wake_soon(now);
        }
        self.insert(
            NewTask {
                id: &derived_id(&format!(
                    "alert/{name}/{event}/{now}/{}",
                    task_id.unwrap_or("-")
                )),
                task: Task::new(now, DEFAULT_MAX_ATTEMPTS),
                method: Method::Post,
                url,
                headers: &[("Content-Type".into(), "application/json".into())],
                body: Some(report.to_string()),
                run_at_ms: now,
                idempotency_key: None,
                fingerprint: "alert",
                callback_url: None,
                callback_for: None,
                capture: false,
                lane: None,
                cron: None,
                alert_for: Some(name),
                wake_call: false,
                report_failures_only: false,
                watch: None,
            },
            now,
        )
    }

    // -- watches --------------------------------------------------------------------------

    /// Judges a watch's poll: done when it matches, over when polling keeps failing or the
    /// deadline passes, otherwise scheduled again. `None` if the poll no longer holds the task.
    fn finish_poll(
        &self,
        row: &TaskRow,
        fence: u64,
        outcome: Outcome,
        body: Option<PollBody<'_>>,
        now: i64,
    ) -> Result<Option<Status>, DbError> {
        let watch = row
            .watch()?
            .ok_or_else(|| DbError(format!("task {} is not a watch", row.id)))?;
        let mut task = row.task()?;
        let mut state = row.watch_state()?;
        let (poll, errors) = judge(&watch.until, &outcome, body, row.watch_errors);
        let (next, matched) = match poll {
            Poll::Matched(value) => {
                state.reason = Some("matched".into());
                state.matched = Some(value);
                (None, true)
            }
            Poll::Fail(reason) => {
                state.reason = Some(reason.into());
                (None, false)
            }
            Poll::NotYet => match next_poll(now, &watch) {
                Some(at) => (Some(at), false),
                None => {
                    state.reason = Some("timed_out".into());
                    (None, false)
                }
            },
        };
        let Ok(status) = task.finish_poll(fence, next, matched, now) else {
            return Ok(None);
        };
        if status == Status::Canceled {
            state.reason = None;
        }
        state.last_status = match outcome {
            Outcome::Http { status, .. } => Some(status),
            _ => None,
        };
        state.last_body = body.map(|b| excerpt(b.text, WATCH_EXCERPT_BYTES));
        self.record_outcome(&row.id, fence, &outcome, now)?;
        self.store_watch(&row.id, errors, &state)?;
        self.save_row(row, &task, now)?;
        Ok(Some(status))
    }

    fn store_watch(&self, task_id: &str, errors: u32, state: &WatchState) -> Result<(), DbError> {
        let state = serde_json::to_string(state).map_err(|e| DbError(e.to_string()))?;
        self.db.exec(
            "UPDATE tasks SET watch_errors = ?, watch_state = ? WHERE id = ?",
            vec![errors.into(), state.into(), task_id.into()],
        )
    }

    // -- secrets --------------------------------------------------------------------------

    fn secret_row(&self, name: &str) -> Result<Option<SecretRow>, DbError> {
        Ok(self
            .db
            .query(
                "SELECT name, sealed, value_bytes, allowed_hosts, created_ms, updated_ms FROM secrets \
                 WHERE name = ?",
                vec![name.into()],
            )?
            .pop())
    }

    /// Stores a secret sealed by the API (the ledger never sees a value), replacing one of the
    /// same name. Work that refers to it uses the new value from its next attempt.
    pub fn put_secret(
        &self,
        name: &str,
        sealed: &str,
        value_bytes: usize,
        allowed_hosts: &[String],
        now: i64,
    ) -> Result<(SecretView, bool), ApiError> {
        self.project_row()?.ok_or_else(no_project)?;
        let existing = self.secret_row(name)?;
        if existing.is_none() {
            let count: Vec<Counts> = self.db.query(
                "SELECT COUNT(*) AS total, 0 AS waiting FROM secrets",
                vec![],
            )?;
            if count.first().map_or(0, |c| c.total) >= MAX_SECRETS {
                return Err(ApiError::new(
                    429,
                    "secret_limit",
                    format!("this project already has {MAX_SECRETS} secrets; delete one first"),
                ));
            }
        }
        let hosts = serde_json::to_string(allowed_hosts).map_err(|e| DbError(e.to_string()))?;
        self.db.exec(
            "INSERT INTO secrets (name, sealed, value_bytes, allowed_hosts, created_ms, updated_ms) \
             VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (name) DO UPDATE SET sealed = excluded.sealed, \
             value_bytes = excluded.value_bytes, allowed_hosts = excluded.allowed_hosts, \
             updated_ms = excluded.updated_ms",
            vec![
                name.into(),
                sealed.into(),
                (value_bytes as i64).into(),
                hosts.into(),
                now.into(),
                now.into(),
            ],
        )?;
        let row = self
            .secret_row(name)?
            .ok_or_else(|| DbError("secret vanished".into()))?;
        Ok((row.view()?, existing.is_none()))
    }

    pub fn secrets(&self) -> Result<SecretList, ApiError> {
        self.project_row()?.ok_or_else(no_project)?;
        let rows: Vec<SecretRow> = self.db.query(
            "SELECT name, sealed, value_bytes, allowed_hosts, created_ms, updated_ms FROM secrets ORDER BY name",
            vec![],
        )?;
        let secrets = rows.iter().map(SecretRow::view).collect::<Result<_, _>>()?;
        Ok(SecretList { secrets })
    }

    /// Removes a secret. Work that still refers to it fails, without an attempt, when it next
    /// comes due (`failure: secret_missing`).
    pub fn delete_secret(&self, name: &str) -> Result<SecretView, ApiError> {
        let row = self.secret_row(name)?.ok_or_else(|| {
            ApiError::not_found(
                "secret_not_found",
                "no secret with that name in this project",
            )
        })?;
        self.db
            .exec("DELETE FROM secrets WHERE name = ?", vec![name.into()])?;
        Ok(row.view()?)
    }

    /// New work may refer only to secrets that exist and allow its host. This is what stops a
    /// tricked agent from sending a secret somewhere else.
    fn check_secrets(&self, headers: &[(String, String)], host: &str) -> Result<(), ApiError> {
        let names = header_references(headers).map_err(|e| ApiError {
            field: Some("headers"),
            ..ApiError::bad_request(e.0)
        })?;
        if names.is_empty() {
            return Ok(());
        }
        let mut secrets = Vec::new();
        for name in names {
            let Some(row) = self.secret_row(&name)? else {
                return Err(ApiError {
                    field: Some("headers"),
                    ..ApiError::new(
                        400,
                        "secret_not_found",
                        format!(
                            "headers refer to {{{{secret:{name}}}}}, but this project has no secret {name}"
                        ),
                    )
                });
            };
            let hosts = row.hosts()?;
            if !host_allowed(&hosts, host) {
                return Err(ApiError {
                    field: Some("url"),
                    ..ApiError::new(
                        400,
                        "secret_host_not_allowed",
                        format!(
                            "secret {name} may not be sent to {host}; it allows {}",
                            hosts.join(", ")
                        ),
                    )
                });
            }
            secrets.push(row);
        }
        if filled_header_bytes(headers, &secrets) > MAX_HEADER_BYTES {
            return Err(ApiError {
                field: Some("headers"),
                ..ApiError::bad_request(format!(
                    "headers exceed {MAX_HEADER_BYTES} bytes once their secrets are filled in"
                ))
            });
        }
        Ok(())
    }

    /// The sealed secrets a task's headers need, or why it can't be sent: a secret is gone
    /// (`secret_missing`), no longer allows its host (`secret_host_not_allowed`), was replaced by
    /// one too long for its headers (`secret_too_large`), or the API can't open secrets at all
    /// (`secret_unavailable`). Storage errors are errors, not reasons to fail the task.
    fn sealed_for(
        &self,
        row: &TaskRow,
    ) -> Result<Result<Vec<SealedSecret>, &'static str>, DbError> {
        let headers = row.headers()?;
        let Ok(names) = header_references(&headers) else {
            return Ok(Err("secret_missing"));
        };
        if names.is_empty() {
            return Ok(Ok(vec![]));
        }
        if !self.secrets_available {
            return Ok(Err("secret_unavailable"));
        }
        let Ok(destination) = runlater_egress::Destination::parse(&row.url) else {
            return Ok(Err("secret_host_not_allowed"));
        };
        let mut secrets = Vec::with_capacity(names.len());
        for name in names {
            let Some(secret) = self.secret_row(&name)? else {
                return Ok(Err("secret_missing"));
            };
            if !host_allowed(&secret.hosts()?, destination.host()) {
                return Ok(Err("secret_host_not_allowed"));
            }
            secrets.push(secret);
        }
        if filled_header_bytes(&headers, &secrets) > MAX_HEADER_BYTES {
            return Ok(Err("secret_too_large"));
        }
        Ok(Ok(secrets
            .into_iter()
            .map(|s| SealedSecret {
                name: s.name,
                sealed: s.sealed,
            })
            .collect()))
    }

    // -- wake -----------------------------------------------------------------------------

    fn wake_row(&self) -> Result<Option<WakeRow>, DbError> {
        Ok(self
            .db
            .query(&format!("SELECT {WAKE_COLUMNS} FROM wake"), vec![])?
            .pop())
    }

    /// Sets where wakes go, replacing any earlier setting and turning the wake back on. A
    /// wake not yet sent is canceled; if the inbox has unread events, a new one is queued.
    pub fn set_wake(&self, request: WakeRequest, now: i64) -> Result<WakeView, ApiError> {
        self.project_row()?.ok_or_else(no_project)?;
        let spec = request
            .validate(now)
            .map_err(|e: WakeRequestError| ApiError {
                field: Some(e.field()),
                ..ApiError::bad_request(e.to_string())
            })?;
        self.check_secrets(&spec.request.headers, spec.request.destination.host())?;
        let headers =
            serde_json::to_string(&spec.request.headers).map_err(|e| DbError(e.to_string()))?;
        self.cancel_pending_wakes(now)?;
        self.db.exec(
            "INSERT INTO wake (id, url, headers, debounce_ms, state, consecutive_failures, \
             covered_seq, created_ms, updated_ms) VALUES (1, ?, ?, ?, 'active', 0, \
             (SELECT inbox_acked_seq FROM project), ?, ?) \
             ON CONFLICT (id) DO UPDATE SET url = excluded.url, headers = excluded.headers, \
             debounce_ms = excluded.debounce_ms, state = 'active', disabled_reason = NULL, \
             consecutive_failures = 0, updated_ms = excluded.updated_ms",
            vec![
                spec.request.destination.as_str().into(),
                headers.into(),
                spec.debounce_ms.into(),
                now.into(),
                now.into(),
            ],
        )?;
        self.wake_soon(now)?;
        Ok(self
            .wake_row()?
            .ok_or_else(|| DbError("wake vanished".into()))?
            .view()?)
    }

    /// Stops wakes. One already being sent finishes; one not yet sent is canceled.
    pub fn delete_wake(&self, now: i64) -> Result<WakeView, ApiError> {
        let wake = self.wake_row()?.ok_or_else(|| {
            ApiError::not_found("wake_not_set", "no wake is set for this project")
        })?;
        self.cancel_pending_wakes(now)?;
        self.db.exec("DELETE FROM wake", vec![])?;
        Ok(wake.view()?)
    }

    fn cancel_pending_wakes(&self, now: i64) -> Result<(), DbError> {
        let pending: Vec<TaskRow> = self.db.query(
            &format!(
                "SELECT {TASK_COLUMNS} FROM tasks \
                 WHERE wake_call = 1 AND status IN ('scheduled', 'retrying')"
            ),
            vec![],
        )?;
        for row in pending {
            let mut task = row.task()?;
            if task.cancel(now).is_ok() {
                self.save_row(&row, &task, now)?;
            }
        }
        Ok(())
    }

    /// Attempts admission promised to work that is waiting: one per task not yet attempted
    /// (scheduled or retrying) and one per callback still owed. Wakes are not counted.
    fn reserved_attempts(&self) -> Result<i64, DbError> {
        let rows: Vec<Counts> = self.db.query(
            &format!(
                "SELECT COALESCE(SUM(status IN ('scheduled', 'retrying')), 0) \
                 + COALESCE(SUM(status IN ('scheduled', 'retrying', 'delivering') \
                 AND {OWES_CALLBACK}), 0) AS total, 0 AS waiting FROM tasks WHERE wake_call = 0"
            ),
            vec![],
        )?;
        Ok(rows.first().map_or(0, |c| c.total))
    }

    /// The inbox has unread events: queue a wake, unless one is already on its way. It goes
    /// no sooner than `debounce` after the last wake sent, so a burst of events shares one.
    fn wake_soon(&self, now: i64) -> Result<(), DbError> {
        let Some(wake) = self.wake_row()? else {
            return Ok(());
        };
        if wake.state != "active" {
            return Ok(());
        }
        let pending: Vec<IdRow> = self.db.query(
            "SELECT id FROM tasks WHERE wake_call = 1 \
             AND status IN ('scheduled', 'retrying', 'delivering') LIMIT 1",
            vec![],
        )?;
        if !pending.is_empty() {
            return Ok(());
        }
        let Some(project) = self.project_row()? else {
            return Ok(());
        };
        if project.inbox_unread == 0 {
            return Ok(());
        }
        let due = wake
            .last_wake_ms
            .map_or(now, |last| now.max(last + wake.debounce_ms));
        // Numbered, not derived from what it covers: a wake canceled by set_wake is followed by
        // one for the same events and time, which must not collide with it.
        self.db
            .exec("UPDATE project SET wakes_queued = wakes_queued + 1", vec![])?;
        let queued: Vec<SeqRow> = self
            .db
            .query("SELECT wakes_queued AS seq FROM project", vec![])?;
        let n = queued.first().map_or(0, |r| r.seq);
        let id = derived_id(&format!("wake/{n}"));
        self.insert(
            NewTask {
                id: &id,
                task: Task::new(due, DEFAULT_MAX_ATTEMPTS),
                method: Method::Post,
                url: &wake.url,
                headers: &wake.headers()?,
                // Rewritten when it is sent, with the count at that moment.
                body: Some(wake_payload(&project.id, project.inbox_unread)),
                run_at_ms: due,
                idempotency_key: None,
                fingerprint: "wake",
                callback_url: None,
                callback_for: None,
                capture: false,
                lane: None,
                cron: None,
                alert_for: None,
                wake_call: true,
                report_failures_only: false,
                watch: None,
            },
            now,
        )?;
        self.db.exec(
            "UPDATE wake SET last_wake_task_id = ?, updated_ms = ?",
            vec![id.as_str().into(), now.into()],
        )
    }

    /// A wake ended. For the current wake setting, success resets the failure count and a
    /// failure counts, enough in a row turning the wake off with an inbox event saying why.
    /// Any wake ending (even one sent before set_wake replaced the setting) checks whether
    /// unread events still need one. Canceled wakes say nothing: whoever canceled them decides.
    fn wake_finished(&self, task_id: &str, status: Status, now: i64) -> Result<(), DbError> {
        let Some(wake) = self.wake_row()? else {
            return Ok(());
        };
        let current = wake.last_wake_task_id.as_deref() == Some(task_id);
        match status {
            Status::Succeeded if current => {
                self.db.exec(
                    "UPDATE wake SET consecutive_failures = 0, updated_ms = ?",
                    vec![now.into()],
                )?;
            }
            Status::Succeeded => {}
            Status::Failed if !current => {}
            Status::Failed => {
                let failures = wake.consecutive_failures + 1;
                if failures >= MAX_WAKE_FAILURES {
                    self.db.exec(
                        "UPDATE wake SET consecutive_failures = ?, state = 'disabled', \
                         disabled_reason = 'wake_failing', updated_ms = ?",
                        vec![failures.into(), now.into()],
                    )?;
                    // Not a wake trigger: the wake is off, and this is why.
                    let seq = self.next_inbox_seq()?;
                    let event = serde_json::json!({
                        "event": "wake.disabled",
                        "reason": "wake_failing",
                        "task_id": task_id,
                        "wake": self.wake_row()?.map(|w| w.view()).transpose()?,
                    });
                    return self.add_to_inbox(seq, &event, now);
                }
                self.db.exec(
                    "UPDATE wake SET consecutive_failures = ?, updated_ms = ?",
                    vec![failures.into(), now.into()],
                )?;
            }
            _ => return Ok(()),
        }
        // A failed wake told the agent nothing, so anything unread needs another; a delivered
        // one covered what had arrived by then.
        if status == Status::Succeeded {
            let newer: Vec<IdRow> = self.db.query(
                "SELECT CAST(seq AS TEXT) AS id FROM inbox WHERE seq > ? \
                 AND seq > (SELECT inbox_acked_seq FROM project) LIMIT 1",
                vec![wake.covered_seq.into()],
            )?;
            if newer.is_empty() {
                return Ok(());
            }
        }
        self.wake_soon(now)
    }

    // -- inbox ----------------------------------------------------------------------------

    /// Unread events, or those after `after`, oldest first. With `ack`, also acks up to the
    /// last one returned. `dropped` is reported as it was before that ack.
    pub fn read_inbox(
        &self,
        after: Option<i64>,
        limit: Option<usize>,
        ack: bool,
    ) -> Result<InboxPage, ApiError> {
        let project = self.project_row()?.ok_or_else(no_project)?;
        if after.is_some_and(|a| a < 0) {
            return Err(ApiError {
                field: Some("after"),
                ..ApiError::bad_request("after must not be negative")
            });
        }
        // Acking up to the last event returned would also ack the unread ones skipped over.
        if ack && after.is_some_and(|a| a > project.inbox_acked_seq) {
            return Err(ApiError {
                field: Some("after"),
                ..ApiError::bad_request(format!(
                    "with ack, after must not be past the acked cursor ({}); \
                     read without ack, or call ack_inbox",
                    project.inbox_acked_seq
                ))
            });
        }
        let limit = limit.unwrap_or(INBOX_DEFAULT_PAGE).clamp(1, INBOX_MAX_PAGE);
        let rows: Vec<InboxRow> = self.db.query(
            "SELECT seq, at_ms, event FROM inbox WHERE seq > ? ORDER BY seq LIMIT ?",
            vec![
                after.unwrap_or(project.inbox_acked_seq).into(),
                (limit as i64).into(),
            ],
        )?;
        let events = rows
            .iter()
            .map(InboxRow::event)
            .collect::<Result<Vec<_>, _>>()?;
        let cursor = match rows.last() {
            Some(last) if ack => self.move_cursor(&project, last.seq)?,
            _ => InboxCursor {
                acked_seq: project.inbox_acked_seq,
                unread: project.inbox_unread,
            },
        };
        Ok(InboxPage {
            events,
            acked_seq: cursor.acked_seq,
            unread: cursor.unread,
            dropped: project.inbox_dropped,
        })
    }

    /// Marks every event up to `up_to` read. The cursor only moves forward, so a session
    /// acking late never undoes a newer one's ack.
    pub fn ack_inbox(&self, up_to: i64) -> Result<InboxCursor, ApiError> {
        let project = self.project_row()?.ok_or_else(no_project)?;
        if up_to < 0 {
            return Err(ApiError {
                field: Some("up_to"),
                ..ApiError::bad_request("up_to must not be negative")
            });
        }
        if up_to > project.inbox_last_seq {
            return Err(ApiError {
                field: Some("up_to"),
                ..ApiError::new(
                    400,
                    "ack_beyond_inbox",
                    format!(
                        "up_to is past the newest event ({}); ack only events you have read",
                        project.inbox_last_seq
                    ),
                )
            });
        }
        Ok(self.move_cursor(&project, up_to)?)
    }

    fn move_cursor(&self, project: &ProjectRow, up_to: i64) -> Result<InboxCursor, DbError> {
        if up_to > project.inbox_acked_seq {
            self.db.exec(
                "UPDATE project SET inbox_acked_seq = ?, inbox_dropped = 0",
                vec![up_to.into()],
            )?;
        }
        let acked_seq = up_to.max(project.inbox_acked_seq);
        let unread: Vec<Counts> = self.db.query(
            "SELECT COUNT(*) AS total, 0 AS waiting FROM inbox WHERE seq > ?",
            vec![acked_seq.into()],
        )?;
        Ok(InboxCursor {
            acked_seq,
            unread: unread.first().map_or(0, |c| c.total),
        })
    }

    /// Hands out the next event number. Separate from [`Self::add_to_inbox`] so a task can
    /// name its own event inside the report it carries.
    fn next_inbox_seq(&self) -> Result<i64, DbError> {
        self.db.exec(
            "UPDATE project SET inbox_last_seq = inbox_last_seq + 1",
            vec![],
        )?;
        let rows: Vec<SeqRow> = self
            .db
            .query("SELECT inbox_last_seq AS seq FROM project", vec![])?;
        rows.first()
            .map(|r| r.seq)
            .ok_or_else(|| DbError("no project for the inbox".into()))
    }

    /// Adds an event. A full inbox makes room by removing its oldest acked event, or if every
    /// event is unread, its oldest one, counted in `dropped`.
    fn add_to_inbox(&self, seq: i64, event: &serde_json::Value, now: i64) -> Result<(), DbError> {
        // A captured body inside the event goes when the task's own copy does.
        let response_until = event["task"]["response"]["body"]
            .is_string()
            .then_some(now + CAPTURE_RETENTION_MS);
        self.db.exec(
            "INSERT INTO inbox (seq, at_ms, event, response_until_ms) VALUES (?, ?, ?, ?)",
            vec![
                seq.into(),
                now.into(),
                event.to_string().into(),
                response_until.into(),
            ],
        )?;
        let held: Vec<Counts> = self
            .db
            .query("SELECT COUNT(*) AS total, 0 AS waiting FROM inbox", vec![])?;
        if held.first().map_or(0, |c| c.total) <= MAX_INBOX_EVENTS {
            return Ok(());
        }
        let oldest: Vec<OldestRow> = self.db.query(
            "SELECT seq, seq > (SELECT inbox_acked_seq FROM project) AS unread FROM inbox \
             ORDER BY unread, seq LIMIT 1",
            vec![],
        )?;
        if let Some(oldest) = oldest.first() {
            self.db
                .exec("DELETE FROM inbox WHERE seq = ?", vec![oldest.seq.into()])?;
            if oldest.unread != 0 {
                self.db.exec(
                    "UPDATE project SET inbox_dropped = inbox_dropped + 1",
                    vec![],
                )?;
            }
        }
        Ok(())
    }

    /// Acked events go a week after they arrived; unread ones after the task retention
    /// period, counted in `dropped`.
    fn purge_inbox(&self, now: i64) -> Result<(), DbError> {
        let Some(project) = self.project_row()? else {
            return Ok(());
        };
        self.db.exec(
            "DELETE FROM inbox WHERE seq <= ? AND at_ms <= ?",
            vec![
                project.inbox_acked_seq.into(),
                (now - INBOX_ACKED_RETENTION_MS).into(),
            ],
        )?;
        let stale: Vec<Counts> = self.db.query(
            "SELECT COUNT(*) AS total, 0 AS waiting FROM inbox WHERE seq > ? AND at_ms <= ?",
            vec![project.inbox_acked_seq.into(), (now - RETENTION_MS).into()],
        )?;
        let stale = stale.first().map_or(0, |c| c.total);
        if stale > 0 {
            self.db.exec(
                "DELETE FROM inbox WHERE seq > ? AND at_ms <= ?",
                vec![project.inbox_acked_seq.into(), (now - RETENTION_MS).into()],
            )?;
            self.db.exec(
                "UPDATE project SET inbox_dropped = inbox_dropped + ?",
                vec![stale.into()],
            )?;
        }
        Ok(())
    }

    /// Removes captured response bodies from inbox events once the task's copy is gone.
    fn expire_inbox_responses(&self, now: i64) -> Result<(), DbError> {
        let due: Vec<InboxRow> = self.db.query(
            "SELECT seq, at_ms, event FROM inbox WHERE response_until_ms <= ?",
            vec![now.into()],
        )?;
        for row in due {
            let mut event: serde_json::Value =
                serde_json::from_str(&row.event).map_err(|e| DbError(e.to_string()))?;
            if let Some(response) = event["task"]["response"].as_object_mut() {
                response.insert("body".into(), serde_json::Value::Null);
            }
            self.db.exec(
                "UPDATE inbox SET event = ?, response_until_ms = NULL WHERE seq = ?",
                vec![event.to_string().into(), row.seq.into()],
            )?;
        }
        Ok(())
    }

    // -- helpers --------------------------------------------------------------------------

    fn lane_clear(&self, task_id: &str) -> Result<bool, DbError> {
        let rows: Vec<IdRow> = self.db.query(
            &format!("SELECT id FROM tasks t WHERE id = ? AND {LANE_CLEAR}"),
            vec![task_id.into()],
        )?;
        Ok(!rows.is_empty())
    }

    fn insert(&self, new: NewTask<'_>, now: i64) -> Result<(), DbError> {
        let NewTask { id, task, .. } = &new;
        let headers = serde_json::to_string(new.headers).map_err(|e| DbError(e.to_string()))?;
        // Callback, run and alert ids are derived, so queueing the same one again is a no-op.
        let derived = new.callback_for.is_some() || new.cron.is_some() || new.alert_for.is_some();
        let verb = if derived {
            "INSERT OR IGNORE"
        } else {
            "INSERT"
        };
        self.db.exec(
            &format!(
                "{verb} INTO tasks (id, status, due_ms, max_attempts, run_attempts, \
             total_attempts, generation, fence, lease_until_ms, cancel_requested, terminal_ms, \
             published_ms, method, url, headers, body, run_at_ms, idempotency_key, fingerprint, \
             callback_url, callback_for, capture, lane, cron, alert_for, wake_call, \
             report_failures_only, watch, created_ms, updated_ms) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
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
                new.callback_url.into(),
                new.callback_for.into(),
                new.capture.into(),
                new.lane.into(),
                new.cron.into(),
                new.alert_for.into(),
                new.wake_call.into(),
                new.report_failures_only.into(),
                new.watch.into(),
                now.into(),
                now.into(),
            ],
        )
    }

    /// Saves a change to a task loaded as `row`; if the change finished the task, queues
    /// its callback in the same transaction.
    fn save_row(&self, row: &TaskRow, task: &Task, now: i64) -> Result<(), DbError> {
        self.save(&row.id, task, now)?;
        let was_finished = parse::<Status>(&row.status)?.is_terminal();
        if !task.status.is_terminal() || was_finished {
            return Ok(());
        }
        if let Some(name) = &row.cron {
            self.run_finished(name, row, task.status, now)?;
        }
        if row.wake_call != 0 {
            self.wake_finished(&row.id, task.status, now)?;
        }
        if row.watch.is_some() || row.wake_call != 0 {
            // Watches and wakes are never sent again, and their headers often carry a token.
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
        if row.report_failures_only != 0 && task.status != Status::Failed {
            return Ok(());
        }
        match row.callback_url.as_deref() {
            Some(INBOX) => self.report_to_inbox(&row.id, now),
            Some(url) => self.queue_callback(&row.id, url, task.generation, now),
            None => Ok(()),
        }
    }

    /// The inbox's version of a callback: the same report, as an event, in this transaction.
    fn report_to_inbox(&self, task_id: &str, now: i64) -> Result<(), DbError> {
        let seq = self.next_inbox_seq()?;
        self.db.exec(
            "UPDATE tasks SET inbox_seq = ? WHERE id = ?",
            vec![seq.into(), task_id.into()],
        )?;
        let row = self
            .task_row(task_id)?
            .ok_or_else(|| DbError(format!("task {task_id} vanished")))?;
        let report = serde_json::json!({ "event": "task.finished", "task": self.full_view(&row)? });
        self.add_to_inbox(seq, &report, now)?;
        self.wake_soon(now)
    }

    /// A callback is an ordinary task: signed, retried and visible through get_task. Admission
    /// reserved an attempt for it, and [`Self::claim`] gives it one try even past the trial.
    fn queue_callback(
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
            NewTask {
                id: &id,
                task: Task::new(now, DEFAULT_MAX_ATTEMPTS),
                method: Method::Post,
                url,
                headers: &[("Content-Type".into(), "application/json".into())],
                body: Some(report.to_string()),
                run_at_ms: now,
                idempotency_key: None,
                fingerprint: "callback",
                callback_url: None,
                callback_for: Some(task_id),
                capture: false,
                lane: None,
                cron: None,
                alert_for: None,
                wake_call: false,
                report_failures_only: false,
                watch: None,
            },
            now,
        )
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

    /// Attempt numbers and fences advance together, so the fence names the attempt row.
    fn record_outcome(
        &self,
        task_id: &str,
        fence: u64,
        outcome: &Outcome,
        now: i64,
    ) -> Result<(), DbError> {
        let http_status = match outcome {
            Outcome::Http { status, .. } => Some(i64::from(*status)),
            _ => None,
        };
        self.db.exec(
            "UPDATE attempts SET finished_ms = ?, outcome = ?, http_status = ? \
             WHERE task_id = ? AND number = ?",
            vec![
                now.into(),
                outcome.label().into(),
                http_status.into(),
                task_id.into(),
                fence.into(),
            ],
        )
    }
}

/// The first `max` bytes of `s`, cut at a character boundary.
fn excerpt(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[derive(Deserialize)]
struct MetaRow {
    value: i64,
}

#[derive(Deserialize)]
struct Counts {
    total: i64,
    waiting: i64,
}

#[derive(Deserialize)]
struct KeyRow {
    id: String,
    fingerprint: String,
}

#[derive(Deserialize)]
struct DueRow {
    id: String,
    generation: u64,
}

#[derive(Deserialize)]
struct IdRow {
    id: String,
}

/// Header bytes as they will be sent: each reference counted at its secret's length.
fn filled_header_bytes(headers: &[(String, String)], secrets: &[SecretRow]) -> usize {
    let pad = "x".repeat(runlater_core::limits::MAX_SECRET_BYTES);
    headers
        .iter()
        .map(|(name, value)| {
            let filled = fill(value, |n| {
                let row = secrets.iter().find(|s| s.name == n)?;
                pad.get(..usize::try_from(row.value_bytes).ok()?)
            });
            name.len() + filled.map_or(value.len(), |f| f.len())
        })
        .sum()
}

/// What a wake sends: only that something is waiting. The agent reads it from the inbox.
fn wake_payload(project_id: &str, unread: i64) -> String {
    serde_json::json!({ "event": "inbox.unread", "project_id": project_id, "unread": unread })
        .to_string()
}

const WAKE_COLUMNS: &str = "url, headers, debounce_ms, state, disabled_reason, \
    consecutive_failures, last_wake_ms, last_wake_task_id, covered_seq, created_ms, updated_ms";

#[derive(Deserialize)]
struct WakeRow {
    url: String,
    headers: String,
    debounce_ms: i64,
    state: String,
    disabled_reason: Option<String>,
    consecutive_failures: i64,
    last_wake_ms: Option<i64>,
    last_wake_task_id: Option<String>,
    covered_seq: i64,
    created_ms: i64,
    updated_ms: i64,
}

impl WakeRow {
    fn headers(&self) -> Result<Vec<(String, String)>, DbError> {
        serde_json::from_str(&self.headers).map_err(|e| DbError(e.to_string()))
    }

    fn view(&self) -> Result<WakeView, DbError> {
        Ok(WakeView {
            url: self.url.clone(),
            header_names: self.headers()?.into_iter().map(|(k, _)| k).collect(),
            debounce_ms: self.debounce_ms,
            state: self.state.clone(),
            disabled_reason: self.disabled_reason.clone(),
            consecutive_failures: self.consecutive_failures,
            last_wake_ms: self.last_wake_ms,
            last_wake_task_id: self.last_wake_task_id.clone(),
            created_ms: self.created_ms,
            updated_ms: self.updated_ms,
        })
    }
}

#[derive(Deserialize)]
struct SeqRow {
    seq: i64,
}

#[derive(Deserialize)]
struct OldestRow {
    seq: i64,
    unread: i64,
}

#[derive(Deserialize)]
struct InboxRow {
    seq: i64,
    at_ms: i64,
    event: String,
}

impl InboxRow {
    fn event(&self) -> Result<serde_json::Value, DbError> {
        let mut event: serde_json::Value =
            serde_json::from_str(&self.event).map_err(|e| DbError(e.to_string()))?;
        if let Some(fields) = event.as_object_mut() {
            fields.insert("seq".into(), self.seq.into());
            fields.insert("at_ms".into(), self.at_ms.into());
        }
        Ok(event)
    }
}

#[derive(Deserialize)]
struct Wake {
    t: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::Memory;
    use runlater_core::limits::LEASE_MS;

    const T0: i64 = 1_767_225_600_000;
    const P: &str = "proj_1";

    fn http(status: u16) -> Outcome {
        Outcome::Http {
            status,
            retry_after_ms: None,
        }
    }

    fn req(url: &str) -> ScheduleRequest {
        ScheduleRequest {
            url: url.into(),
            ..Default::default()
        }
    }

    fn setup(db: &Memory) -> Ledger<'_, Memory> {
        let l = Ledger::open(db).unwrap();
        l.create_project(P, "secret", T0).unwrap();
        l
    }

    fn deliver(l: &Ledger<'_, Memory>, d: &Dispatch, now: i64) -> Delivery {
        match l.claim(&d.task_id, d.generation, now).unwrap() {
            Claimed::Deliver(d) => *d,
            other => panic!("expected delivery, got {other:?}"),
        }
    }

    #[test]
    fn opening_twice_is_harmless() {
        let db = Memory::new();
        setup(&db);
        assert!(Ledger::open(&db).is_ok());
    }

    #[test]
    fn creates_a_project_once() {
        let db = Memory::new();
        let l = Ledger::open(&db).unwrap();
        assert_eq!(l.project().unwrap_err().code, "project_not_found");

        let (p, created) = l.create_project(P, "s1", T0).unwrap();
        assert!(created);
        assert_eq!(p.attempts_remaining, i64::from(TRIAL_ATTEMPTS));
        assert_eq!(p.expires_ms, T0 + TRIAL_DURATION_MS);

        let (again, created) = l.create_project(P, "s2", T0 + 5).unwrap();
        assert!(!created);
        assert_eq!(
            again, p,
            "a second create must not refill or rekey the trial"
        );
    }

    #[test]
    fn credit_adds_attempts_once_and_extends_expiry() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(req("https://a.example.com/"), "t1", T0).unwrap();
        db.0.execute("UPDATE project SET attempts_used = 30", [])
            .unwrap();

        let (p, added) = l.add_credit("cs_test_1", 50_000, T0 + 10).unwrap();
        assert!(added);
        assert!(p.paid);
        assert_eq!(
            p.attempts_remaining,
            70 + 50_000,
            "unused trial attempts carry over"
        );
        assert_eq!(p.expires_ms, T0 + 10 + CREDIT_VALIDITY_MS);

        let (again, added) = l.add_credit("cs_test_1", 50_000, T0 + 20).unwrap();
        assert!(!added);
        assert_eq!(again, p, "a repeated payment must not add twice");

        let (two, _) = l.add_credit("cs_test_2", 50_000, T0 + 30).unwrap();
        assert_eq!(two.attempts_remaining, 70 + 100_000);
        assert_eq!(two.expires_ms, T0 + 30 + CREDIT_VALIDITY_MS);
    }

    #[test]
    fn credit_after_expiry_drops_expired_attempts_and_reopens_the_project() {
        let db = Memory::new();
        let l = setup(&db);
        let end = T0 + TRIAL_DURATION_MS;
        assert_eq!(
            l.schedule(req("https://a.example.com/"), "t1", end)
                .unwrap_err()
                .code,
            "trial_expired"
        );
        let (p, _) = l.add_credit("cs_1", 50_000, end).unwrap();
        assert_eq!(
            p.attempts_remaining, 50_000,
            "expired trial attempts are not revived"
        );
        l.schedule(req("https://a.example.com/"), "t1", end)
            .unwrap();
    }

    #[test]
    fn credit_restarts_crons_stopped_by_expiry() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(daily("backup"), T0).unwrap();
        let end = T0 + TRIAL_DURATION_MS;
        l.fire_crons(end).unwrap();
        assert_eq!(l.cron("backup").unwrap().next_run_ms, None);

        l.add_credit("cs_1", 50_000, end).unwrap();
        assert!(l.cron("backup").unwrap().next_run_ms.unwrap() > end);
        assert!(l.next_wake().unwrap().is_some());
    }

    #[test]
    fn credit_rejects_bad_arguments() {
        let db = Memory::new();
        let l = Ledger::open(&db).unwrap();
        assert_eq!(
            l.add_credit("cs_1", 1, T0).unwrap_err().code,
            "project_not_found"
        );
        let l = setup(&db);
        for (id, attempts, field) in [
            ("", 1, "payment_id"),
            ("cs 1", 1, "payment_id"),
            ("cs_1", 0, "attempts"),
            ("cs_1", MAX_CREDIT_ATTEMPTS + 1, "attempts"),
        ] {
            assert_eq!(
                l.add_credit(id, attempts, T0).unwrap_err().field,
                Some(field)
            );
        }
        assert!(!l.project().unwrap().paid);
    }

    #[test]
    fn a_paid_project_holds_more_tasks() {
        let db = Memory::new();
        let l = setup(&db);
        l.add_credit("cs_1", 5_000, T0).unwrap();
        for i in 0..=MAX_RETAINED_TASKS {
            l.schedule(req("https://a.example.com/"), &format!("t{i}"), T0)
                .unwrap();
        }
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
    fn full_delivery_cycle_succeeds() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(req("https://a.example.com/"), "t1", T0).unwrap();

        let due = l.publish_due(P, T0).unwrap();
        assert_eq!(
            due,
            [Dispatch {
                project_id: P.into(),
                task_id: "t1".into(),
                generation: 1
            }]
        );
        assert!(
            l.publish_due(P, T0 + 1).unwrap().is_empty(),
            "not republished right away"
        );

        let d = deliver(&l, &due[0], T0 + 10);
        assert_eq!(
            (d.attempt, d.fence, d.signing_secret.as_str()),
            (1, 1, "secret")
        );
        assert_eq!(l.task("t1").unwrap().status, Status::Delivering);
        assert_eq!(l.project().unwrap().attempts_used, 1);

        assert_eq!(
            l.finish("t1", 1, http(200), T0 + 20, 0.0).unwrap(),
            Some(Status::Succeeded)
        );
        let t = l.task("t1").unwrap();
        assert_eq!(t.status, Status::Succeeded);
        assert_eq!(t.finished_ms, Some(T0 + 20));
        assert_eq!(
            t.attempts.unwrap(),
            [AttemptView {
                number: 1,
                started_ms: T0 + 10,
                finished_ms: Some(T0 + 20),
                outcome: Some("http_200".into()),
                http_status: Some(200),
            }]
        );
    }

    #[test]
    fn retries_get_republished_with_a_new_generation() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(req("https://a.example.com/"), "t1", T0).unwrap();
        let due = l.publish_due(P, T0).unwrap();
        deliver(&l, &due[0], T0);
        assert_eq!(
            l.finish("t1", 1, http(503), T0, 0.0).unwrap(),
            Some(Status::Retrying)
        );

        let t = l.task("t1").unwrap();
        let next = t.next_attempt_ms.unwrap();
        assert_eq!(next, T0 + 5_000);
        assert_eq!(l.next_wake().unwrap(), Some(next));
        assert!(l.publish_due(P, next - 1).unwrap().is_empty());

        let again = l.publish_due(P, next).unwrap();
        assert_eq!(again[0].generation, 2);
        assert_eq!(
            l.claim("t1", 1, next).unwrap(),
            Claimed::Skip("stale"),
            "old message ignored"
        );
        let d = deliver(&l, &again[0], next);
        assert_eq!(d.attempt, 2);
    }

    #[test]
    fn republishes_unclaimed_messages() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(req("https://a.example.com/"), "t1", T0).unwrap();
        assert_eq!(l.publish_due(P, T0).unwrap().len(), 1);
        assert_eq!(l.next_wake().unwrap(), Some(T0 + REPUBLISH_MS));
        assert_eq!(l.publish_due(P, T0 + REPUBLISH_MS).unwrap().len(), 1);
    }

    #[test]
    fn ignores_duplicate_and_stale_messages() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(req("https://a.example.com/"), "t1", T0).unwrap();
        let due = l.publish_due(P, T0).unwrap();
        deliver(&l, &due[0], T0);
        assert_eq!(l.claim("t1", 1, T0).unwrap(), Claimed::Skip("stale"));
        assert_eq!(
            l.claim("nope", 1, T0).unwrap(),
            Claimed::Skip("task_missing")
        );
        assert_eq!(l.finish("t1", 99, http(200), T0, 0.0).unwrap(), None);
        assert_eq!(
            l.project().unwrap().attempts_used,
            1,
            "duplicates are not charged"
        );
    }

    #[test]
    fn recovers_lost_attempts() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(req("https://a.example.com/"), "t1", T0).unwrap();
        let due = l.publish_due(P, T0).unwrap();
        deliver(&l, &due[0], T0);
        assert_eq!(l.next_wake().unwrap(), Some(T0 + LEASE_MS));

        assert_eq!(l.recover(T0 + LEASE_MS - 1, || 0.0).unwrap(), 0);
        assert_eq!(l.recover(T0 + LEASE_MS, || 0.0).unwrap(), 1);
        let t = l.task("t1").unwrap();
        assert_eq!(t.status, Status::Retrying);
        assert_eq!(
            t.attempts.unwrap()[0].outcome.as_deref(),
            Some("lease_expired")
        );
        assert_eq!(
            l.finish("t1", 1, http(200), T0 + LEASE_MS + 1, 0.0)
                .unwrap(),
            None
        );
    }

    #[test]
    fn fails_waiting_tasks_when_the_budget_runs_out() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(req("https://a.example.com/"), "t1", T0).unwrap();
        db.0.execute("UPDATE project SET attempts_used = attempt_budget", [])
            .unwrap();
        let due = l.publish_due(P, T0).unwrap();
        assert_eq!(
            l.claim("t1", 1, T0).unwrap(),
            Claimed::Skip("budget_exhausted")
        );
        let t = l.task("t1").unwrap();
        assert_eq!((t.status, t.attempt_count), (Status::Failed, 0));
        assert_eq!(
            l.claim("t1", due[0].generation, T0).unwrap(),
            Claimed::Skip("stale")
        );
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
    fn purges_after_retention() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(req("https://a.example.com/"), "t1", T0).unwrap();
        let due = l.publish_due(P, T0).unwrap();
        deliver(&l, &due[0], T0);
        l.finish("t1", 1, http(200), T0, 0.0).unwrap();
        assert_eq!(l.next_wake().unwrap(), Some(T0 + RETENTION_MS));

        assert_eq!(l.purge(T0 + RETENTION_MS - 1).unwrap(), 0);
        assert_eq!(l.purge(T0 + RETENTION_MS).unwrap(), 1);
        assert_eq!(l.task("t1").unwrap_err().code, "task_not_found");
        assert_eq!(l.next_wake().unwrap(), None);
    }

    fn with_callback(url: &str) -> ScheduleRequest {
        ScheduleRequest {
            callback_url: Some("https://cb.example.com/done".into()),
            ..req(url)
        }
    }

    #[test]
    fn migrates_a_v1_ledger_in_place() {
        let db = Memory::new();
        for statement in &SCHEMA[..SCHEMA.len() - 1] {
            db.0.execute(statement, []).unwrap();
        }
        db.0.execute(
            "INSERT INTO tasks (id, status, due_ms, max_attempts, run_attempts, total_attempts, \
             generation, fence, cancel_requested, method, url, headers, run_at_ms, fingerprint, \
             created_ms, updated_ms) VALUES ('old', 'scheduled', 1, 5, 0, 0, 1, 0, 0, 'POST', \
             'https://a.example.com/', '[]', 1, 'f', 1, 1)",
            [],
        )
        .unwrap();
        let l = Ledger::open(&db).unwrap();
        let t = l.task("old").unwrap();
        assert_eq!((t.callback_url, t.callback_task_id), (None, None));
        Ledger::open(&db).expect("migrations run once");
        let version: i64 =
            db.0.query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
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
        let cb_id = t.callback_task_id.clone().expect("callback queued");
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
        assert_eq!(l.project().unwrap().attempts_used, 2);
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
        l.schedule(later.clone(), "c", T0).unwrap();
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
        assert_eq!(l.project().unwrap().attempts_remaining, 0);

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
    fn callback_ids_are_stable_and_distinct() {
        assert_eq!(callback_id("t1", 1), callback_id("t1", 1));
        assert_ne!(callback_id("t1", 1), callback_id("t1", 2));
        assert_ne!(callback_id("t1", 1), callback_id("t2", 1));
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
    fn resumes_a_migration_that_stopped_halfway() {
        let db = Memory::new();
        for statement in SCHEMA {
            db.0.execute(statement, []).unwrap();
        }
        db.0.execute(MIGRATIONS[0][0], []).unwrap();
        let l = Ledger::open(&db).unwrap();
        l.create_project(P, "s", T0).unwrap();
        l.schedule(with_callback("https://a.example.com/"), "t1", T0)
            .unwrap();
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

    fn in_lane(lane: &str, run_at_ms: i64) -> ScheduleRequest {
        ScheduleRequest {
            lane: Some(lane.into()),
            run_at_ms: Some(run_at_ms),
            max_attempts: Some(2),
            ..req("https://a.example.com/")
        }
    }

    fn published(l: &Ledger<'_, Memory>, now: i64) -> Vec<String> {
        l.publish_due(P, now)
            .unwrap()
            .into_iter()
            .map(|d| d.task_id)
            .collect()
    }

    /// Claims and finishes the one published task `id` with `status`.
    fn run(l: &Ledger<'_, Memory>, id: &str, status: u16, now: i64) {
        let row = l.task(id).unwrap();
        let generation: u64 =
            l.db.query::<DueRow>(
                "SELECT id, generation FROM tasks WHERE id = ?",
                vec![id.into()],
            )
            .unwrap()[0]
                .generation;
        assert!(row.next_attempt_ms.is_some(), "{id} is waiting");
        let a = deliver(
            l,
            &Dispatch {
                project_id: P.into(),
                task_id: id.into(),
                generation,
            },
            now,
        );
        l.finish(id, a.fence, http(status), now, 0.0).unwrap();
    }

    #[test]
    fn a_lane_runs_one_task_at_a_time_in_order() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(in_lane("pay", T0), "a", T0).unwrap();
        l.schedule(in_lane("pay", T0), "b", T0).unwrap();
        l.schedule(in_lane("other", T0), "x", T0).unwrap();
        l.schedule(req("https://a.example.com/"), "free", T0)
            .unwrap();
        assert_eq!(published(&l, T0), ["a", "x", "free"]);
        assert_eq!(l.task("b").unwrap().lane.as_deref(), Some("pay"));

        // a retries: it keeps the lane through its backoff.
        run(&l, "a", 503, T0);
        let a = l.task("a").unwrap();
        assert_eq!(a.status, Status::Retrying);
        let retry_at = a.next_attempt_ms.unwrap();
        assert!(published(&l, retry_at - 1).is_empty());
        assert_eq!(
            l.next_wake().unwrap(),
            Some(retry_at),
            "only a's retry wakes the lane; b is blocked"
        );
        assert_eq!(published(&l, retry_at), ["a"]);
        run(&l, "a", 200, retry_at);
        assert_eq!(published(&l, retry_at), ["b"]);
    }

    #[test]
    fn a_failed_or_canceled_task_frees_its_lane() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(in_lane("pay", T0), "a", T0).unwrap();
        l.schedule(in_lane("pay", T0), "b", T0).unwrap();
        l.schedule(in_lane("pay", T0), "c", T0).unwrap();
        assert_eq!(published(&l, T0), ["a"]);
        run(&l, "a", 404, T0);
        assert_eq!(l.task("a").unwrap().status, Status::Failed);
        assert_eq!(published(&l, T0 + 1), ["b"]);
        l.cancel("b", T0 + 2).unwrap();
        assert_eq!(published(&l, T0 + 2), ["c"]);

        // A redriven task rejoins the lane as due now: ahead of d, which is due later.
        l.schedule(in_lane("pay", T0 + 10), "d", T0 + 3).unwrap();
        l.redrive("a", T0 + 4).unwrap();
        run(&l, "c", 200, T0 + 5);
        assert_eq!(published(&l, T0 + 6), ["a"]);
        run(&l, "a", 200, T0 + 6);
        assert_eq!(published(&l, T0 + 10), ["d"]);
    }

    #[test]
    fn a_later_task_waits_for_an_earlier_one_due_first() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(in_lane("pay", T0 + 60_000), "later", T0)
            .unwrap();
        l.schedule(in_lane("pay", T0 + 1_000), "sooner", T0)
            .unwrap();
        assert_eq!(l.next_wake().unwrap(), Some(T0 + 1_000));
        assert_eq!(published(&l, T0 + 1_000), ["sooner"]);
        run(&l, "sooner", 200, T0 + 1_000);
        assert_eq!(l.next_wake().unwrap(), Some(T0 + 60_000));
    }

    #[test]
    fn a_stale_claim_cannot_jump_the_lane() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(in_lane("pay", T0), "a", T0).unwrap();
        l.schedule(in_lane("pay", T0), "b", T0).unwrap();
        assert_eq!(published(&l, T0), ["a"]);
        // A duplicate message for b (e.g. from before a was scheduled) must not run b.
        let gen_b = 1;
        assert_eq!(l.claim("b", gen_b, T0).unwrap(), Claimed::Skip("lane_busy"));
        run(&l, "a", 200, T0);
        assert_eq!(published(&l, T0 + 1), ["b"]);
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

    fn daily(name: &str) -> CronRequest {
        CronRequest {
            name: name.into(),
            schedule: "0 6 * * *".into(),
            url: "https://a.example.com/backup".into(),
            max_attempts: Some(1),
            alert_url: Some("https://alerts.example.com/hook".into()),
            ..Default::default()
        }
    }

    const SIX: i64 = T0 + 6 * 3_600_000;
    const DAY: i64 = 86_400_000;

    fn alerts(db: &Memory, name: &str) -> Vec<serde_json::Value> {
        let mut stmt =
            db.0.prepare("SELECT body FROM tasks WHERE alert_for = ? ORDER BY seq")
                .unwrap();
        stmt.query_map([name], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|b| serde_json::from_str(&b.unwrap()).unwrap())
            .collect()
    }

    fn run_cron_task(l: &Ledger<'_, Memory>, name: &str, status: u16, now: i64) -> String {
        let id = l.cron(name).unwrap().last_run_task_id.unwrap();
        let d = l
            .publish_due(P, now)
            .unwrap()
            .into_iter()
            .find(|d| d.task_id == id)
            .unwrap();
        let a = deliver(l, &d, now);
        l.finish(&id, a.fence, http(status), now, 0.0).unwrap();
        id
    }

    #[test]
    fn a_cron_runs_on_schedule_as_ordinary_tasks() {
        let db = Memory::new();
        let l = setup(&db);
        let (c, created) = l.put_cron(daily("backup"), T0).unwrap();
        assert!(created);
        assert_eq!((c.next_run_ms, c.health), (Some(SIX), Health::Pending));
        assert_eq!(l.next_wake().unwrap(), Some(SIX));

        assert_eq!(l.fire_crons(SIX - 1).unwrap(), 0);
        assert_eq!(l.fire_crons(SIX).unwrap(), 1);
        assert_eq!(l.fire_crons(SIX).unwrap(), 0, "once per slot");
        let c = l.cron("backup").unwrap();
        assert_eq!(c.next_run_ms, Some(SIX + DAY));
        let run = l.task(c.last_run_task_id.as_deref().unwrap()).unwrap();
        assert_eq!(run.cron.as_deref(), Some("backup"));
        assert_eq!(
            (run.url.as_str(), run.run_at_ms),
            ("https://a.example.com/backup", SIX)
        );

        run_cron_task(&l, "backup", 200, SIX + 5);
        let c = l.cron("backup").unwrap();
        assert_eq!(
            (c.health, c.last_success_ms),
            (Health::Healthy, Some(SIX + 5))
        );
        assert!(alerts(&db, "backup").is_empty(), "no news is good news");
    }

    #[test]
    fn alerts_when_a_cron_starts_failing_and_when_it_recovers() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(daily("backup"), T0).unwrap();
        l.fire_crons(SIX).unwrap();
        let failed_run = run_cron_task(&l, "backup", 500, SIX);
        let c = l.cron("backup").unwrap();
        assert_eq!(c.health, Health::Failing);
        assert_eq!(c.health_reason.as_deref(), Some("run_failed"));
        assert_eq!(c.failing_since_ms, Some(SIX));

        // Failing again does not alert again.
        l.fire_crons(SIX + DAY).unwrap();
        run_cron_task(&l, "backup", 500, SIX + DAY);
        l.fire_crons(SIX + 2 * DAY).unwrap();
        run_cron_task(&l, "backup", 200, SIX + 2 * DAY);

        let sent = alerts(&db, "backup");
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0]["event"], "cron.failing");
        assert_eq!(sent[0]["reason"], "run_failed");
        assert_eq!(sent[0]["task_id"], failed_run.as_str());
        assert_eq!(sent[0]["cron"]["health"], "failing");
        assert_eq!(sent[1]["event"], "cron.recovered");
        assert_eq!(sent[1]["cron"]["health"], "healthy");

        let alert_id: String =
            db.0.query_row(
                "SELECT id FROM tasks WHERE alert_for = 'backup' ORDER BY seq LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let alert = l.task(&alert_id).unwrap();
        assert_eq!(alert.url, "https://alerts.example.com/hook");
        assert_eq!(alert.alert_for.as_deref(), Some("backup"));
    }

    #[test]
    fn missed_slots_are_not_made_up() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(daily("backup"), T0).unwrap();
        assert_eq!(l.fire_crons(SIX + 3 * DAY + 1).unwrap(), 1);
        assert_eq!(l.cron("backup").unwrap().next_run_ms, Some(SIX + 4 * DAY));
    }

    #[test]
    fn a_run_that_cannot_start_is_a_failure() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(daily("backup"), T0).unwrap();
        db.0.execute("UPDATE project SET attempts_used = attempt_budget", [])
            .unwrap();
        assert_eq!(l.fire_crons(SIX).unwrap(), 0);
        let c = l.cron("backup").unwrap();
        assert_eq!(c.health_reason.as_deref(), Some("budget_exhausted"));
        assert_eq!(c.last_run_task_id, None);
        assert_eq!(c.next_run_ms, Some(SIX + DAY), "keeps trying on schedule");
        // The alert still gets its one try.
        let sent = alerts(&db, "backup");
        assert_eq!(sent[0]["reason"], "budget_exhausted");
        let d = &l.publish_due(P, SIX).unwrap()[0];
        assert!(matches!(
            l.claim(&d.task_id, d.generation, SIX).unwrap(),
            Claimed::Deliver(_)
        ));
    }

    #[test]
    fn a_cron_stops_when_the_trial_ends() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(daily("backup"), T0).unwrap();
        let end = T0 + TRIAL_DURATION_MS;
        l.fire_crons(end).unwrap();
        let c = l.cron("backup").unwrap();
        assert_eq!(c.next_run_ms, None);
        assert_eq!(c.health_reason.as_deref(), Some("trial_expired"));
        assert_eq!(alerts(&db, "backup").len(), 1);
        assert_eq!(
            l.put_cron(daily("other"), end).unwrap_err().code,
            "trial_expired"
        );
    }

    #[test]
    fn replaces_lists_and_deletes_crons() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(daily("b"), T0).unwrap();
        l.put_cron(daily("a"), T0).unwrap();
        let hourly = CronRequest {
            schedule: "0 * * * *".into(),
            ..daily("b")
        };
        let (c, created) = l.put_cron(hourly, T0 + 1).unwrap();
        assert!(!created);
        assert_eq!(
            (c.schedule.as_str(), c.next_run_ms),
            ("0 * * * *", Some(T0 + 3_600_000))
        );
        let names: Vec<_> = l
            .crons()
            .unwrap()
            .crons
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["a", "b"]);

        l.delete_cron("b").unwrap();
        assert_eq!(l.cron("b").unwrap_err().code, "cron_not_found");
        assert_eq!(l.delete_cron("b").unwrap_err().status, 404);
        assert_eq!(l.next_wake().unwrap(), Some(SIX));

        let err = l
            .put_cron(
                CronRequest {
                    schedule: "*/5 * * * *".into(),
                    ..daily("c")
                },
                T0,
            )
            .unwrap_err();
        assert_eq!((err.status, err.field), (400, Some("schedule")));
        for i in 1..MAX_CRONS {
            l.put_cron(daily(&format!("n{i}")), T0).unwrap();
        }
        assert_eq!(
            l.put_cron(daily("full"), T0).unwrap_err().code,
            "cron_limit"
        );
        l.put_cron(daily("a"), T0).expect("replacing is not adding");
    }

    #[test]
    fn a_deleted_crons_run_still_finishes_quietly() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(daily("backup"), T0).unwrap();
        l.fire_crons(SIX).unwrap();
        let id = l.cron("backup").unwrap().last_run_task_id.unwrap();
        l.delete_cron("backup").unwrap();
        let d = &l.publish_due(P, SIX).unwrap()[0];
        let a = deliver(&l, d, SIX);
        l.finish(&id, a.fence, http(500), SIX, 0.0).unwrap();
        assert_eq!(l.task(&id).unwrap().status, Status::Failed);
        assert!(alerts(&db, "backup").is_empty());
    }

    #[test]
    fn a_recreated_cron_ignores_its_predecessors_runs() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(daily("backup"), T0).unwrap();
        l.fire_crons(SIX).unwrap();
        let old = l.cron("backup").unwrap().last_run_task_id.unwrap();
        l.delete_cron("backup").unwrap();
        l.put_cron(daily("backup"), SIX + 1).unwrap();
        let d = &l.publish_due(P, SIX + 2).unwrap()[0];
        assert_eq!(d.task_id, old);
        let a = deliver(&l, d, SIX + 2);
        l.finish(&old, a.fence, http(500), SIX + 2, 0.0).unwrap();
        assert_eq!(l.cron("backup").unwrap().health, Health::Pending);
        assert!(alerts(&db, "backup").is_empty());
    }

    #[test]
    fn an_older_run_cannot_overrule_a_newer_one() {
        let db = Memory::new();
        let l = setup(&db);
        let hourly = CronRequest {
            schedule: "0 * * * *".into(),
            max_attempts: Some(2),
            ..daily("h")
        };
        l.put_cron(hourly, T0).unwrap();
        let hour = 3_600_000;
        l.fire_crons(T0 + hour).unwrap();
        let first = l.cron("h").unwrap().last_run_task_id.unwrap();
        l.fire_crons(T0 + 2 * hour).unwrap();
        let second = l.cron("h").unwrap().last_run_task_id.unwrap();
        let due = l.publish_due(P, T0 + 2 * hour).unwrap();
        let attempt = |id: &str| {
            let d = due.iter().find(|d| d.task_id == id).unwrap();
            deliver(&l, d, T0 + 2 * hour)
        };
        let (a1, a2) = (attempt(&first), attempt(&second));
        // The newer run succeeds first; the older one failing afterwards is old news.
        l.finish(&second, a2.fence, http(200), T0 + 2 * hour + 1, 0.0)
            .unwrap();
        l.finish(&first, a1.fence, http(404), T0 + 2 * hour + 2, 0.0)
            .unwrap();
        assert_eq!(l.cron("h").unwrap().health, Health::Healthy);
        assert!(alerts(&db, "h").is_empty());
    }

    #[test]
    fn the_trial_ending_stops_crons_promptly() {
        let db = Memory::new();
        let l = setup(&db);
        let yearly = CronRequest {
            schedule: "0 0 1 1 *".into(),
            ..daily("yearly")
        };
        l.put_cron(yearly, T0).unwrap();
        let end = T0 + TRIAL_DURATION_MS;
        assert_eq!(
            l.next_wake().unwrap(),
            Some(end),
            "wakes at the trial's end"
        );
        l.fire_crons(end).unwrap();
        let c = l.cron("yearly").unwrap();
        assert_eq!(
            (c.next_run_ms, c.health_reason.as_deref()),
            (None, Some("trial_expired"))
        );
        assert_eq!(l.next_wake().unwrap(), alerts_wake(&db));
    }

    // -- watches ----------------------------------------------------------------------

    const MIN: i64 = 60_000;

    fn watching(extra: serde_json::Value) -> WatchRequest {
        let mut v = serde_json::json!({
            "url": "https://api.example.com/jobs/1",
            "headers": { "Authorization": "Bearer secret-token" },
            "every": "5m",
            "until": { "path": "/status", "in": ["succeeded", "failed"] },
            "deadline": "1h",
        });
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        serde_json::from_value(v).unwrap()
    }

    /// Runs the watch's due poll at `now` with this response.
    fn poll(
        l: &Ledger<'_, Memory>,
        id: &str,
        outcome: Outcome,
        body: Option<&str>,
        now: i64,
    ) -> Status {
        let d = l
            .publish_due(P, now)
            .unwrap()
            .into_iter()
            .find(|d| d.task_id == id)
            .expect("poll is due");
        let a = deliver(l, &d, now);
        assert!(a.read_body, "polls read the response");
        let read = body.map(|text| ResponseRead {
            body: Some(PollBody {
                text,
                truncated: false,
            }),
            headers: &[],
        });
        l.finish_response(id, a.fence, outcome, read, now, 0.0)
            .unwrap()
            .expect("current poll")
    }

    fn watch_view(l: &Ledger<'_, Memory>, id: &str) -> WatchView {
        l.task(id).unwrap().watch.expect("a watch")
    }

    #[test]
    fn a_watch_polls_until_it_matches_then_calls_back() {
        let db = Memory::new();
        let l = setup(&db);
        let req = watching(serde_json::json!({ "callback_url": "https://cb.example.com/done" }));
        let (t, created) = l.watch(req, "w1", T0).unwrap();
        assert!(created);
        assert_eq!(
            (t.status, t.method, t.next_attempt_ms),
            (Status::Scheduled, Method::Get, Some(T0))
        );
        let w = t.watch.unwrap();
        assert_eq!(
            (w.every_ms, w.deadline_ms, w.reason),
            (5 * MIN, T0 + 60 * MIN, None)
        );

        let running = Some(r#"{"status":"running"}"#);
        assert_eq!(poll(&l, "w1", http(200), running, T0), Status::Scheduled);
        assert_eq!(l.task("w1").unwrap().next_attempt_ms, Some(T0 + 5 * MIN));
        assert_eq!(watch_view(&l, "w1").last_body.as_deref(), running);
        assert!(
            l.publish_due(P, T0 + 5 * MIN - 1).unwrap().is_empty(),
            "waits for the next poll"
        );
        assert_eq!(
            poll(&l, "w1", http(200), running, T0 + 5 * MIN),
            Status::Scheduled
        );

        let done = r#"{"status":"succeeded","output":"s3://bucket/out"}"#;
        assert_eq!(
            poll(&l, "w1", http(200), Some(done), T0 + 10 * MIN),
            Status::Succeeded
        );
        let t = l.task("w1").unwrap();
        assert_eq!(t.attempt_count, 3);
        let w = t.watch.unwrap();
        assert_eq!(w.reason.as_deref(), Some("matched"));
        assert_eq!(w.matched, Some(serde_json::json!("succeeded")));
        assert_eq!(
            (w.last_status, w.last_body.as_deref()),
            (Some(200), Some(done))
        );
        assert_eq!(l.project().unwrap().attempts_used, 3);

        // The token is gone; the name stays.
        assert_eq!(t.header_names, ["Authorization"]);
        let stored: Vec<IdRow> = db
            .query("SELECT headers AS id FROM tasks WHERE id = 'w1'", vec![])
            .unwrap();
        assert!(!stored[0].id.contains("secret-token"), "{}", stored[0].id);

        let cb = l.task(t.callback_task_id.as_deref().unwrap()).unwrap();
        let d = l.publish_due(P, T0 + 10 * MIN).unwrap();
        assert_eq!(d[0].task_id, cb.id);
        let report: serde_json::Value =
            serde_json::from_str(deliver(&l, &d[0], T0 + 10 * MIN).body.as_deref().unwrap())
                .unwrap();
        assert_eq!(report["task"]["status"], "succeeded");
        assert_eq!(report["task"]["watch"]["matched"], "succeeded");
        assert_eq!(report["task"]["watch"]["last_body"], done);
    }

    #[test]
    fn lists_leave_out_the_last_response_body() {
        let db = Memory::new();
        let l = setup(&db);
        l.watch(watching(serde_json::json!({})), "w1", T0).unwrap();
        poll(&l, "w1", http(200), Some(r#"{"status":"running"}"#), T0);
        let listed = l
            .list(None, None, None, None)
            .unwrap()
            .tasks
            .remove(0)
            .watch
            .unwrap();
        assert_eq!((listed.last_status, listed.last_body), (Some(200), None));
        assert!(watch_view(&l, "w1").last_body.is_some(), "get_task has it");
    }

    #[test]
    fn three_failed_polls_in_a_row_end_a_watch() {
        let db = Memory::new();
        let l = setup(&db);
        l.watch(watching(serde_json::json!({})), "w1", T0).unwrap();
        assert_eq!(poll(&l, "w1", http(503), None, T0), Status::Scheduled);
        assert_eq!(
            poll(&l, "w1", Outcome::Timeout, None, T0 + 5 * MIN),
            Status::Scheduled
        );
        assert_eq!(watch_view(&l, "w1").consecutive_errors, 2);
        // A good answer resets the count.
        let running = Some(r#"{"status":"running"}"#);
        assert_eq!(
            poll(&l, "w1", http(200), running, T0 + 10 * MIN),
            Status::Scheduled
        );
        assert_eq!(watch_view(&l, "w1").consecutive_errors, 0);
        for n in 0..2 {
            let at = T0 + (15 + 5 * n) * MIN;
            assert_eq!(poll(&l, "w1", http(502), None, at), Status::Scheduled);
        }
        assert_eq!(
            poll(&l, "w1", Outcome::Network, None, T0 + 25 * MIN),
            Status::Failed
        );
        let w = watch_view(&l, "w1");
        assert_eq!(
            (w.reason.as_deref(), w.consecutive_errors),
            (Some("poll_failing"), 3)
        );
    }

    #[test]
    fn a_rejected_poll_ends_a_watch_at_once() {
        let db = Memory::new();
        let l = setup(&db);
        l.watch(watching(serde_json::json!({})), "w1", T0).unwrap();
        assert_eq!(
            poll(&l, "w1", http(401), Some("bad token"), T0),
            Status::Failed
        );
        let w = watch_view(&l, "w1");
        assert_eq!(w.reason.as_deref(), Some("poll_rejected"));
        assert_eq!(
            (w.last_status, w.last_body.as_deref()),
            (Some(401), Some("bad token"))
        );
    }

    #[test]
    fn a_watch_times_out_after_polling_at_its_deadline() {
        let db = Memory::new();
        let l = setup(&db);
        l.watch(
            watching(serde_json::json!({ "every": "5m", "deadline": "12m" })),
            "w1",
            T0,
        )
        .unwrap();
        let running = Some(r#"{"status":"running"}"#);
        for at in [T0, T0 + 5 * MIN, T0 + 10 * MIN] {
            assert_eq!(poll(&l, "w1", http(200), running, at), Status::Scheduled);
        }
        assert_eq!(l.task("w1").unwrap().next_attempt_ms, Some(T0 + 12 * MIN));
        assert_eq!(
            poll(&l, "w1", http(200), running, T0 + 12 * MIN),
            Status::Failed
        );
        assert_eq!(watch_view(&l, "w1").reason.as_deref(), Some("timed_out"));
        assert_eq!(l.task("w1").unwrap().attempt_count, 4);
    }

    #[test]
    fn a_lost_poll_counts_as_a_failed_one() {
        let db = Memory::new();
        let l = setup(&db);
        l.watch(watching(serde_json::json!({})), "w1", T0).unwrap();
        let d = &l.publish_due(P, T0).unwrap()[0];
        deliver(&l, d, T0);
        l.recover(T0 + LEASE_MS, || 0.0).unwrap();
        let t = l.task("w1").unwrap();
        assert_eq!(
            (t.status, t.next_attempt_ms),
            (Status::Scheduled, Some(T0 + LEASE_MS + 5 * MIN))
        );
        assert_eq!(watch_view(&l, "w1").consecutive_errors, 1);
        assert_eq!(
            t.attempts.unwrap()[0].outcome.as_deref(),
            Some("lease_expired")
        );
    }

    #[test]
    fn a_watch_can_be_canceled_but_not_redriven() {
        let db = Memory::new();
        let l = setup(&db);
        l.watch(watching(serde_json::json!({})), "w1", T0).unwrap();
        poll(&l, "w1", http(200), Some("{}"), T0);
        let t = l.cancel("w1", T0 + 1).unwrap();
        assert_eq!(t.status, Status::Canceled);
        assert_eq!(t.watch.unwrap().reason, None);

        l.watch(watching(serde_json::json!({})), "w2", T0).unwrap();
        poll(&l, "w2", http(404), None, T0);
        assert_eq!(
            l.redrive("w2", T0 + 1).unwrap_err().code,
            "watch_not_redrivable"
        );
    }

    #[test]
    fn a_watch_ends_when_the_trial_runs_out_of_attempts() {
        let db = Memory::new();
        let l = setup(&db);
        l.watch(watching(serde_json::json!({})), "w1", T0).unwrap();
        poll(&l, "w1", http(200), Some("{}"), T0);
        db.exec("UPDATE project SET attempts_used = attempt_budget", vec![])
            .unwrap();
        let d = &l.publish_due(P, T0 + 5 * MIN).unwrap()[0];
        assert_eq!(
            l.claim(&d.task_id, d.generation, T0 + 5 * MIN).unwrap(),
            Claimed::Skip("budget_exhausted")
        );
        let t = l.task("w1").unwrap();
        assert_eq!(t.status, Status::Failed);
        assert_eq!(t.watch.unwrap().reason.as_deref(), Some("budget_exhausted"));
    }

    #[test]
    fn watch_requests_are_checked() {
        let db = Memory::new();
        let l = setup(&db);
        let e = l
            .watch(watching(serde_json::json!({ "every": "1m" })), "w1", T0)
            .unwrap_err();
        assert_eq!((e.status, e.field), (400, Some("every")));
        let e = l
            .watch(
                watching(serde_json::json!({ "until": { "path": "/s" } })),
                "w1",
                T0,
            )
            .unwrap_err();
        assert_eq!(e.field, Some("until"));
        let e = l
            .watch(
                watching(serde_json::json!({ "every": "1d", "deadline": "7d" })),
                "w1",
                T0 + 1,
            )
            .unwrap_err();
        assert_eq!(e.field, Some("deadline"), "past the trial's end");

        let keyed = || watching(serde_json::json!({ "idempotency_key": "job-1" }));
        let (first, created) = l.watch(keyed(), "w1", T0).unwrap();
        assert!(created);
        let (again, created) = l.watch(keyed(), "w2", T0 + 1_000).unwrap();
        assert!(!created);
        assert_eq!(again.id, first.id);
        let changed = watching(serde_json::json!({ "idempotency_key": "job-1", "every": "10m" }));
        assert_eq!(
            l.watch(changed, "w3", T0).unwrap_err().code,
            "idempotency_conflict"
        );
    }

    /// The only thing left to wake for: the alert just queued.
    fn alerts_wake(db: &Memory) -> Option<i64> {
        db.0.query_row(
            "SELECT MIN(due_ms) FROM tasks WHERE alert_for IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    // -- capture and the inbox -------------------------------------------------------------

    fn captured(extra: serde_json::Value) -> ScheduleRequest {
        let mut r = serde_json::json!({ "url": "https://a.example.com/", "capture": true });
        r.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(r).unwrap()
    }

    /// Delivers `id` now and finishes it with this response.
    fn answer(
        l: &Ledger<'_, Memory>,
        id: &str,
        status: u16,
        body: &str,
        headers: &[(&str, &str)],
        now: i64,
    ) -> Option<Status> {
        let d = l
            .publish_due(P, now)
            .unwrap()
            .into_iter()
            .find(|d| d.task_id == id)
            .expect("task is due");
        let a = deliver(l, &d, now);
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let read = ResponseRead {
            body: Some(PollBody {
                text: body,
                truncated: false,
            }),
            headers: &headers,
        };
        l.finish_response(id, a.fence, http(status), Some(read), now, 0.0)
            .unwrap()
    }

    #[test]
    fn capture_keeps_the_last_response() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(captured(serde_json::json!({})), "t1", T0)
            .unwrap();
        let d = &l.publish_due(P, T0).unwrap()[0];
        let a = deliver(&l, d, T0);
        assert!(a.read_body && a.capture);

        let busy = [("Retry-After", "5"), ("X-Secret", "s3cr3t")];
        answer_fenced(&l, "t1", a.fence, 503, r#"{"error":"busy"}"#, &busy, T0);
        let r = l.task("t1").unwrap().response.expect("captured");
        assert_eq!((r.attempt, r.status), (1, 503));
        assert_eq!(r.body.as_deref(), Some(r#"{"error":"busy"}"#));
        assert_eq!(
            r.headers.into_iter().collect::<Vec<_>>(),
            [("retry-after".to_string(), "5".to_string())],
            "only the allowlisted headers"
        );

        // The next attempt replaces it; a long body is cut to 16 KiB.
        let big = "x".repeat(CAPTURE_BODY_BYTES + 10);
        let retry = l.task("t1").unwrap().next_attempt_ms.unwrap();
        let length = big.len().to_string();
        answer(
            &l,
            "t1",
            200,
            &big,
            &[("Content-Type", "text/plain"), ("Content-Length", &length)],
            retry,
        );
        let t = l.task("t1").unwrap();
        assert_eq!(t.status, Status::Succeeded);
        let r = t.response.unwrap();
        assert_eq!((r.attempt, r.status, r.truncated), (2, 200, true));
        assert_eq!(r.body.unwrap().len(), CAPTURE_BODY_BYTES);
        assert_eq!(r.body_bytes, Some(big.len() as u64));
        assert_eq!(r.headers["content-type"], "text/plain");

        let listed = l.list(None, None, None, None).unwrap().tasks.remove(0);
        assert!(listed.capture);
        let r = listed.response.unwrap();
        assert_eq!((r.status, r.body), (200, None), "lists leave out the body");
    }

    fn answer_fenced(
        l: &Ledger<'_, Memory>,
        id: &str,
        fence: u64,
        status: u16,
        body: &str,
        headers: &[(&str, &str)],
        now: i64,
    ) {
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let read = ResponseRead {
            body: Some(PollBody {
                text: body,
                truncated: false,
            }),
            headers: &headers,
        };
        l.finish_response(id, fence, http(status), Some(read), now, 0.0)
            .unwrap()
            .expect("current attempt");
    }

    #[test]
    fn without_capture_no_response_is_kept() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(req("https://a.example.com/"), "t1", T0).unwrap();
        let d = &l.publish_due(P, T0).unwrap()[0];
        assert!(!deliver(&l, d, T0).read_body);
        answer_fenced(&l, "t1", 1, 200, "ok", &[], T0);
        let t = l.task("t1").unwrap();
        assert!(!t.capture);
        assert_eq!(t.response, None);
    }

    #[test]
    fn captured_responses_go_a_week_after_the_task_finishes() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(captured(serde_json::json!({})), "t1", T0)
            .unwrap();
        answer(&l, "t1", 200, "token=abc", &[], T0);
        assert_eq!(l.next_wake().unwrap(), Some(T0 + CAPTURE_RETENTION_MS));
        l.purge(T0 + CAPTURE_RETENTION_MS - 1).unwrap();
        assert!(l.task("t1").unwrap().response.is_some());
        l.purge(T0 + CAPTURE_RETENTION_MS).unwrap();
        let t = l.task("t1").unwrap();
        assert_eq!((t.capture, t.response), (true, None));
        assert_eq!(l.next_wake().unwrap(), Some(T0 + RETENTION_MS));
    }

    #[test]
    fn a_failed_capture_keeps_its_response_through_a_redrive() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(captured(serde_json::json!({ "max_attempts": 1 })), "t1", T0)
            .unwrap();
        answer(&l, "t1", 422, r#"{"error":"email taken"}"#, &[], T0);
        assert_eq!(l.task("t1").unwrap().status, Status::Failed);
        let t = l.redrive("t1", T0 + 10).unwrap();
        assert_eq!(t.response.unwrap().status, 422);
    }

    fn inbox(l: &Ledger<'_, Memory>) -> InboxPage {
        l.read_inbox(None, None, false).unwrap()
    }

    #[test]
    fn a_finished_task_reports_to_the_inbox() {
        let db = Memory::new();
        let l = setup(&db);
        let (t, _) = l
            .schedule(
                captured(serde_json::json!({ "callback_url": "inbox" })),
                "t1",
                T0,
            )
            .unwrap();
        assert_eq!(
            (t.callback_url.as_deref(), t.inbox_seq),
            (Some("inbox"), None)
        );
        assert_eq!(inbox(&l).events, Vec::<serde_json::Value>::new());

        answer(
            &l,
            "t1",
            200,
            r#"{"status":"failed","step":"migrate"}"#,
            &[],
            T0 + 5,
        );
        let t = l.task("t1").unwrap();
        assert_eq!(
            (t.callback_task_id, t.inbox_seq),
            (None, Some(1)),
            "no callback task"
        );
        assert_eq!(l.project().unwrap().inbox_unread, 1);
        assert_eq!(l.project().unwrap().attempts_used, 1);

        let page = inbox(&l);
        assert_eq!((page.acked_seq, page.unread, page.dropped), (0, 1, 0));
        let e = &page.events[0];
        assert_eq!(
            (e["seq"].as_i64(), e["at_ms"].as_i64()),
            (Some(1), Some(T0 + 5))
        );
        assert_eq!(e["event"], "task.finished");
        assert_eq!(e["task"]["id"], "t1");
        assert_eq!(e["task"]["inbox_seq"], 1);
        assert_eq!(e["task"]["response"]["status"], 200);
        assert_eq!(
            e["task"]["response"]["body"],
            r#"{"status":"failed","step":"migrate"}"#
        );
        assert_eq!(e["task"]["attempts"][0]["http_status"], 200);

        // Reading does not move the cursor; acking does.
        assert_eq!(inbox(&l).events.len(), 1);
        assert_eq!(
            l.ack_inbox(1).unwrap(),
            InboxCursor {
                acked_seq: 1,
                unread: 0
            }
        );
        assert_eq!(inbox(&l).events.len(), 0);
        assert_eq!(l.project().unwrap().inbox_unread, 0);
        // Acked events can still be replayed for a while.
        assert_eq!(l.read_inbox(Some(0), None, false).unwrap().events.len(), 1);
    }

    #[test]
    fn every_finish_adds_an_event() {
        let db = Memory::new();
        let l = setup(&db);
        let to_inbox = |extra: serde_json::Value| {
            let mut r =
                serde_json::json!({ "url": "https://a.example.com/", "callback_url": "inbox" });
            r.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            serde_json::from_value::<ScheduleRequest>(r).unwrap()
        };
        l.schedule(to_inbox(serde_json::json!({ "delay": "1h" })), "c", T0)
            .unwrap();
        l.cancel("c", T0).unwrap();
        l.cancel("c", T0 + 1).unwrap();
        l.schedule(to_inbox(serde_json::json!({ "max_attempts": 1 })), "f", T0)
            .unwrap();
        answer(&l, "f", 500, "", &[], T0);
        l.redrive("f", T0 + 10).unwrap();
        answer(&l, "f", 500, "", &[], T0 + 10);

        let events = inbox(&l).events;
        let seen: Vec<_> = events
            .iter()
            .map(|e| {
                (
                    e["seq"].as_i64().unwrap(),
                    e["task"]["id"].as_str().unwrap(),
                    e["task"]["status"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [(1, "c", "canceled"), (2, "f", "failed"), (3, "f", "failed")]
        );
        assert_eq!(l.task("f").unwrap().inbox_seq, Some(3));
    }

    #[test]
    fn inbox_reports_cost_no_attempt_and_survive_the_budget() {
        let db = Memory::new();
        let l = setup(&db);
        db.0.execute("UPDATE project SET attempt_budget = 2", [])
            .unwrap();
        let to_inbox = || ScheduleRequest {
            callback_url: Some(INBOX.into()),
            ..req("https://a.example.com/")
        };
        // A URL callback would reserve a second attempt; the inbox does not.
        l.schedule(to_inbox(), "t1", T0).unwrap();
        l.schedule(to_inbox(), "t2", T0).unwrap();
        assert_eq!(
            l.schedule(to_inbox(), "t3", T0).unwrap_err().code,
            "budget_exhausted"
        );

        // Abandoned for lack of budget still reports.
        db.0.execute("UPDATE project SET attempts_used = 2", [])
            .unwrap();
        let d = l.publish_due(P, T0).unwrap().remove(0);
        assert_eq!(
            l.claim(&d.task_id, d.generation, T0).unwrap(),
            Claimed::Skip("budget_exhausted")
        );
        let e = &inbox(&l).events[0];
        assert_eq!(e["task"]["id"], d.task_id.as_str());
        assert_eq!(e["task"]["status"], "failed");
    }

    #[test]
    fn the_inbox_cursor_only_moves_forward() {
        let db = Memory::new();
        let l = setup(&db);
        for i in 0..5 {
            let seq = l.next_inbox_seq().unwrap();
            l.add_to_inbox(seq, &serde_json::json!({ "event": "test", "i": i }), T0)
                .unwrap();
        }
        let e = l.ack_inbox(6).unwrap_err();
        assert_eq!(
            (e.status, e.code, e.field),
            (400, "ack_beyond_inbox", Some("up_to"))
        );
        assert_eq!(l.ack_inbox(-1).unwrap_err().field, Some("up_to"));
        assert_eq!(l.ack_inbox(3).unwrap().acked_seq, 3);
        assert_eq!(
            l.ack_inbox(1).unwrap(),
            InboxCursor {
                acked_seq: 3,
                unread: 2
            },
            "a late ack changes nothing"
        );

        // read with ack: acks up to the last event returned, no further.
        let page = l.read_inbox(None, Some(1), true).unwrap();
        assert_eq!(page.events[0]["seq"], 4);
        assert_eq!((page.acked_seq, page.unread), (4, 1));
        let page = l.read_inbox(None, Some(500), true).unwrap();
        assert_eq!((page.events.len(), page.acked_seq, page.unread), (1, 5, 0));
        let page = l.read_inbox(None, None, true).unwrap();
        assert_eq!(
            (page.events.len(), page.acked_seq),
            (0, 5),
            "nothing left to ack"
        );
        assert_eq!(
            l.read_inbox(Some(-1), None, false).unwrap_err().field,
            Some("after")
        );
        assert_eq!(
            l.read_inbox(Some(0), Some(0), false).unwrap().events.len(),
            1,
            "limit clamps to 1"
        );
    }

    #[test]
    fn a_full_inbox_drops_acked_events_first_and_says_so() {
        let db = Memory::new();
        let l = setup(&db);
        let add = |n: i64| {
            for _ in 0..n {
                let seq = l.next_inbox_seq().unwrap();
                l.add_to_inbox(seq, &serde_json::json!({ "event": "test" }), T0)
                    .unwrap();
            }
        };
        add(MAX_INBOX_EVENTS);
        l.ack_inbox(2).unwrap();
        add(2);
        let first = l.read_inbox(Some(0), Some(1), false).unwrap();
        assert_eq!(
            (first.events[0]["seq"].as_i64(), first.dropped),
            (Some(3), 0)
        );

        add(1);
        let page = l.read_inbox(None, Some(1), false).unwrap();
        assert_eq!((page.events[0]["seq"].as_i64(), page.dropped), (Some(4), 1));
        assert_eq!(page.unread, MAX_INBOX_EVENTS);
        // Moving the cursor resets the count, but the reply that moved it still shows it.
        assert_eq!(l.read_inbox(None, Some(1), true).unwrap().dropped, 1);
        assert_eq!(inbox(&l).dropped, 0);
    }

    #[test]
    fn old_inbox_events_are_purged() {
        let db = Memory::new();
        let l = setup(&db);
        for at in [T0, T0 + 1] {
            let seq = l.next_inbox_seq().unwrap();
            l.add_to_inbox(seq, &serde_json::json!({ "event": "test" }), at)
                .unwrap();
        }
        l.ack_inbox(1).unwrap();
        assert_eq!(l.next_wake().unwrap(), Some(T0 + INBOX_ACKED_RETENTION_MS));
        l.purge(T0 + INBOX_ACKED_RETENTION_MS).unwrap();
        let page = l.read_inbox(Some(0), None, false).unwrap();
        assert_eq!(page.events.len(), 1, "the acked event is gone");
        assert_eq!(l.next_wake().unwrap(), Some(T0 + 1 + RETENTION_MS));
        l.purge(T0 + 1 + RETENTION_MS).unwrap();
        let page = inbox(&l);
        assert_eq!((page.events.len(), page.unread, page.dropped), (0, 0, 1));
        assert_eq!(l.next_wake().unwrap(), None);
    }

    #[test]
    fn a_cron_can_alert_to_the_inbox() {
        let db = Memory::new();
        let l = setup(&db);
        let cron = CronRequest {
            alert_url: Some(INBOX.into()),
            ..daily("backup")
        };
        assert_eq!(
            l.put_cron(cron, T0).unwrap().0.alert_url.as_deref(),
            Some("inbox")
        );
        l.fire_crons(SIX).unwrap();
        let run = run_cron_task(&l, "backup", 500, SIX);
        assert_eq!(
            alerts(&db, "backup"),
            Vec::<serde_json::Value>::new(),
            "no alert task"
        );
        let e = &inbox(&l).events[0];
        assert_eq!(e["event"], "cron.failing");
        assert_eq!(e["reason"], "run_failed");
        assert_eq!(e["task_id"], run.as_str());
        assert_eq!(e["cron"]["name"], "backup");
    }

    #[test]
    fn a_watch_can_report_to_the_inbox() {
        let db = Memory::new();
        let l = setup(&db);
        l.watch(
            watching(serde_json::json!({ "callback_url": "inbox" })),
            "w1",
            T0,
        )
        .unwrap();
        poll(&l, "w1", http(200), Some(r#"{"status":"succeeded"}"#), T0);
        let e = &inbox(&l).events[0];
        assert_eq!(e["task"]["watch"]["reason"], "matched");
        assert_eq!(l.task("w1").unwrap().callback_task_id, None);
    }

    #[test]
    fn an_unread_body_still_replaces_the_last_capture() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(captured(serde_json::json!({})), "t1", T0)
            .unwrap();
        answer(&l, "t1", 503, "busy", &[], T0);
        // The next attempt is answered, but its body never arrives in time.
        let retry = l.task("t1").unwrap().next_attempt_ms.unwrap();
        let d = l.publish_due(P, retry).unwrap().remove(0);
        let a = deliver(&l, &d, retry);
        let headers = [("content-type".to_string(), "application/json".to_string())];
        let read = ResponseRead {
            body: None,
            headers: &headers,
        };
        l.finish_response("t1", a.fence, http(200), Some(read), retry, 0.0)
            .unwrap();
        let t = l.task("t1").unwrap();
        assert_eq!(t.status, Status::Succeeded);
        let r = t.response.unwrap();
        assert_eq!((r.attempt, r.status, r.truncated), (2, 200, true));
        assert_eq!(r.body, None);
        assert_eq!(r.headers["content-type"], "application/json");
    }

    #[test]
    fn content_length_of_an_encoded_body_is_not_its_size() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(captured(serde_json::json!({})), "t1", T0)
            .unwrap();
        answer(
            &l,
            "t1",
            200,
            "hello, world",
            &[("Content-Length", "5"), ("Content-Encoding", "gzip")],
            T0,
        );
        let r = l.task("t1").unwrap().response.unwrap();
        assert_eq!((r.body_bytes, r.truncated), (None, false));
        assert!(!r.headers.contains_key("content-encoding"));
    }

    #[test]
    fn captured_bodies_leave_inbox_events_with_the_task() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(
            captured(serde_json::json!({ "callback_url": "inbox" })),
            "t1",
            T0,
        )
        .unwrap();
        answer(&l, "t1", 200, "token=abc", &[], T0);
        assert_eq!(l.next_wake().unwrap(), Some(T0 + CAPTURE_RETENTION_MS));
        l.purge(T0 + CAPTURE_RETENTION_MS).unwrap();
        let e = &inbox(&l).events[0];
        assert_eq!(e["task"]["response"]["status"], 200, "the status stays");
        assert_eq!(e["task"]["response"]["body"], serde_json::Value::Null);
        assert_eq!(l.task("t1").unwrap().response, None);
        assert_eq!(l.next_wake().unwrap(), Some(T0 + RETENTION_MS));
    }

    #[test]
    fn ack_never_skips_unread_events() {
        let db = Memory::new();
        let l = setup(&db);
        for _ in 0..6 {
            let seq = l.next_inbox_seq().unwrap();
            l.add_to_inbox(seq, &serde_json::json!({ "event": "test" }), T0)
                .unwrap();
        }
        let e = l.read_inbox(Some(4), None, true).unwrap_err();
        assert_eq!((e.status, e.field), (400, Some("after")));
        assert_eq!(inbox(&l).acked_seq, 0);
        assert_eq!(l.read_inbox(Some(4), None, false).unwrap().events.len(), 2);
        // From at or before the cursor, ack covers only what was returned.
        l.ack_inbox(2).unwrap();
        let page = l.read_inbox(Some(1), Some(2), true).unwrap();
        assert_eq!(
            (page.events[0]["seq"].as_i64(), page.acked_seq),
            (Some(2), 3)
        );
    }

    // -- wake -----------------------------------------------------------------------------

    fn waking(extra: serde_json::Value) -> WakeRequest {
        let mut r = serde_json::json!({
            "url": "https://agents.example.com/trigger",
            "headers": { "Authorization": "Bearer t0ken" },
        });
        r.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(r).unwrap()
    }

    /// Schedules a task that reports to the inbox and lets it succeed at `now`.
    fn finish_to_inbox(l: &Ledger<'_, Memory>, id: &str, now: i64) {
        let r = ScheduleRequest {
            callback_url: Some(INBOX.into()),
            ..req("https://a.example.com/")
        };
        l.schedule(r, id, now).unwrap();
        answer(l, id, 200, "", &[], now);
    }

    fn wake_id(l: &Ledger<'_, Memory>) -> Option<String> {
        l.project().unwrap().wake.and_then(|w| w.last_wake_task_id)
    }

    /// When the pending wake is due.
    fn wake_due(l: &Ledger<'_, Memory>) -> Option<i64> {
        l.task(&wake_id(l)?).unwrap().next_attempt_ms
    }

    /// Sends the pending wake at `now` and answers it with `status`.
    fn send_wake(l: &Ledger<'_, Memory>, status: u16, now: i64) -> Delivery {
        let id = wake_id(l).expect("a wake was queued");
        let d = l
            .publish_due(P, now)
            .unwrap()
            .into_iter()
            .find(|d| d.task_id == id)
            .expect("the wake is due");
        let a = deliver(l, &d, now);
        l.finish(&id, a.fence, http(status), now, 0.0).unwrap();
        a
    }

    fn wake_tasks(db: &Memory) -> i64 {
        db.0.query_row("SELECT COUNT(*) FROM tasks WHERE wake_call = 1", [], |r| {
            r.get(0)
        })
        .unwrap()
    }

    #[test]
    fn no_wake_without_set_wake() {
        let db = Memory::new();
        let l = setup(&db);
        finish_to_inbox(&l, "t1", T0);
        assert_eq!(wake_tasks(&db), 0);
        assert_eq!(l.project().unwrap().wake, None);
        assert_eq!(l.delete_wake(T0).unwrap_err().code, "wake_not_set");
    }

    #[test]
    fn an_inbox_event_wakes_the_agent_with_only_a_count() {
        let db = Memory::new();
        let l = setup(&db);
        let w = l.set_wake(waking(serde_json::json!({})), T0).unwrap();
        assert_eq!((w.state.as_str(), w.debounce_ms), ("active", 15 * 60_000));
        assert_eq!(w.header_names, ["Authorization"]);
        assert_eq!(wake_tasks(&db), 0, "nothing unread yet");

        finish_to_inbox(&l, "t1", T0 + 5);
        let a = send_wake(&l, 202, T0 + 5);
        assert_eq!(a.url, "https://agents.example.com/trigger");
        assert_eq!(a.method, Method::Post);
        assert!(a
            .headers
            .contains(&("Authorization".to_string(), "Bearer t0ken".to_string())));
        let payload: serde_json::Value = serde_json::from_str(a.body.as_deref().unwrap()).unwrap();
        assert_eq!(
            payload,
            serde_json::json!({ "event": "inbox.unread", "project_id": P, "unread": 1 })
        );
        let t = l.task(&wake_id(&l).unwrap()).unwrap();
        assert!(t.wake_call);
        assert_eq!(t.status, Status::Succeeded);
        assert_eq!(t.header_names, ["Authorization"], "values are never echoed");
        assert_eq!(l.project().unwrap().wake.unwrap().consecutive_failures, 0);
    }

    #[test]
    fn a_burst_of_events_shares_one_debounced_wake() {
        let db = Memory::new();
        let l = setup(&db);
        l.set_wake(waking(serde_json::json!({ "debounce": "10m" })), T0)
            .unwrap();
        finish_to_inbox(&l, "t1", T0);
        send_wake(&l, 200, T0);
        // Two more while the window is open: one wake, at its end.
        finish_to_inbox(&l, "t2", T0 + 60_000);
        finish_to_inbox(&l, "t3", T0 + 120_000);
        assert_eq!(wake_tasks(&db), 2);
        assert_eq!(wake_due(&l), Some(T0 + 600_000));
        let a = send_wake(&l, 200, T0 + 600_000);
        let payload: serde_json::Value = serde_json::from_str(a.body.as_deref().unwrap()).unwrap();
        assert_eq!(payload["unread"], 3, "the count when it is sent");
    }

    #[test]
    fn a_wake_due_after_the_agent_read_everything_is_dropped() {
        let db = Memory::new();
        let l = setup(&db);
        l.set_wake(waking(serde_json::json!({})), T0).unwrap();
        finish_to_inbox(&l, "t1", T0);
        send_wake(&l, 200, T0);
        finish_to_inbox(&l, "t2", T0 + 1);
        l.read_inbox(None, None, true).unwrap();
        let id = wake_id(&l).unwrap();
        let due = T0 + 15 * 60_000;
        let d = l
            .publish_due(P, due)
            .unwrap()
            .into_iter()
            .find(|d| d.task_id == id)
            .unwrap();
        let used = l.project().unwrap().attempts_used;
        assert_eq!(
            l.claim(&d.task_id, d.generation, due).unwrap(),
            Claimed::Skip("inbox_read")
        );
        assert_eq!(l.task(&id).unwrap().status, Status::Canceled);
        assert_eq!(l.project().unwrap().attempts_used, used, "costs nothing");
    }

    #[test]
    fn events_after_a_wake_was_sent_get_the_next_one() {
        let db = Memory::new();
        let l = setup(&db);
        l.set_wake(waking(serde_json::json!({ "debounce": "1m" })), T0)
            .unwrap();
        finish_to_inbox(&l, "t1", T0);
        let id = wake_id(&l).unwrap();
        let d = l
            .publish_due(P, T0)
            .unwrap()
            .into_iter()
            .find(|d| d.task_id == id)
            .unwrap();
        let a = deliver(&l, &d, T0);
        // While the wake is in flight, another event arrives; the agent may miss it.
        finish_to_inbox(&l, "t2", T0 + 1);
        assert_eq!(wake_tasks(&db), 1, "no second wake while one is in flight");
        l.finish(&id, a.fence, http(200), T0 + 2, 0.0).unwrap();
        assert_eq!(wake_tasks(&db), 2);
        assert_eq!(
            wake_due(&l),
            Some(T0 + 60_000),
            "a minute after it was sent"
        );

        // A delivered wake with nothing new after it queues nothing more.
        send_wake(&l, 200, T0 + 60_000);
        assert_eq!(wake_tasks(&db), 2);
    }

    #[test]
    fn a_wake_that_keeps_failing_turns_itself_off() {
        let db = Memory::new();
        let l = setup(&db);
        l.set_wake(waking(serde_json::json!({ "debounce": "1m" })), T0)
            .unwrap();
        finish_to_inbox(&l, "t1", T0);
        let mut now = T0;
        for failures in 1..=MAX_WAKE_FAILURES {
            let id = wake_id(&l).unwrap();
            // A 404 fails at once, without retries.
            send_wake(&l, 404, now);
            assert_eq!(l.task(&id).unwrap().status, Status::Failed);
            let w = l.project().unwrap().wake.unwrap();
            assert_eq!(w.consecutive_failures, failures);
            if let Some(due) = wake_due(&l) {
                now = due;
            }
        }
        let w = l.project().unwrap().wake.unwrap();
        assert_eq!(
            (w.state.as_str(), w.disabled_reason.as_deref()),
            ("disabled", Some("wake_failing"))
        );
        assert_eq!(wake_tasks(&db), MAX_WAKE_FAILURES, "no more wakes");
        let events = l.read_inbox(None, None, false).unwrap().events;
        let last = events.last().unwrap();
        assert_eq!(last["event"], "wake.disabled");
        assert_eq!(last["wake"]["state"], "disabled");

        finish_to_inbox(&l, "t2", now + 1);
        assert_eq!(wake_tasks(&db), MAX_WAKE_FAILURES);
        // Setting it again turns it back on, and wakes for what is waiting.
        let w = l
            .set_wake(waking(serde_json::json!({ "debounce": "1m" })), now + 2)
            .unwrap();
        assert_eq!((w.state.as_str(), w.consecutive_failures), ("active", 0));
        assert_eq!(wake_tasks(&db), MAX_WAKE_FAILURES + 1);
    }

    #[test]
    fn delete_wake_cancels_the_one_not_yet_sent() {
        let db = Memory::new();
        let l = setup(&db);
        l.set_wake(waking(serde_json::json!({})), T0).unwrap();
        finish_to_inbox(&l, "t1", T0);
        send_wake(&l, 200, T0);
        finish_to_inbox(&l, "t2", T0 + 1);
        let pending = wake_id(&l).unwrap();
        let gone = l.delete_wake(T0 + 2).unwrap();
        assert_eq!(gone.url, "https://agents.example.com/trigger");
        assert_eq!(l.task(&pending).unwrap().status, Status::Canceled);
        assert_eq!(l.project().unwrap().wake, None);
        finish_to_inbox(&l, "t3", T0 + 3);
        assert_eq!(wake_tasks(&db), 2);
    }

    #[test]
    fn a_wake_gets_one_try_past_the_budget() {
        let db = Memory::new();
        let l = setup(&db);
        l.set_wake(waking(serde_json::json!({})), T0).unwrap();
        db.0.execute("UPDATE project SET attempt_budget = 1", [])
            .unwrap();
        finish_to_inbox(&l, "t1", T0);
        assert_eq!(l.project().unwrap().attempts_remaining, 0);
        let id = wake_id(&l).unwrap();
        let d = l
            .publish_due(P, T0)
            .unwrap()
            .into_iter()
            .find(|d| d.task_id == id)
            .unwrap();
        let a = deliver(&l, &d, T0);
        assert_eq!(
            l.finish(&id, a.fence, http(503), T0, 0.0).unwrap(),
            Some(Status::Failed),
            "one try, no retries"
        );
    }

    #[test]
    fn set_wake_checks_its_request_and_needs_a_project() {
        let db = Memory::new();
        let l = Ledger::open(&db).unwrap();
        assert_eq!(
            l.set_wake(waking(serde_json::json!({})), T0)
                .unwrap_err()
                .code,
            "project_not_found"
        );
        l.create_project(P, "secret", T0).unwrap();
        let e = l
            .set_wake(waking(serde_json::json!({ "debounce": "10s" })), T0)
            .unwrap_err();
        assert_eq!((e.status, e.field), (400, Some("debounce")));
        let e = l
            .set_wake(
                waking(serde_json::json!({ "url": "https://localhost/x" })),
                T0,
            )
            .unwrap_err();
        assert_eq!(e.field, Some("url"));
        // Unread events already waiting get a wake as soon as it is set.
        let seq = l.next_inbox_seq().unwrap();
        l.add_to_inbox(seq, &serde_json::json!({ "event": "test" }), T0)
            .unwrap();
        l.set_wake(waking(serde_json::json!({})), T0).unwrap();
        assert_eq!(wake_tasks(&db), 1);
    }

    #[test]
    fn replacing_the_wake_while_one_is_in_flight_still_wakes_for_later_events() {
        let db = Memory::new();
        let l = setup(&db);
        l.set_wake(waking(serde_json::json!({ "debounce": "1m" })), T0)
            .unwrap();
        finish_to_inbox(&l, "t1", T0);
        let old = wake_id(&l).unwrap();
        let d = l
            .publish_due(P, T0)
            .unwrap()
            .into_iter()
            .find(|d| d.task_id == old)
            .unwrap();
        let a = deliver(&l, &d, T0);
        finish_to_inbox(&l, "t2", T0 + 1);
        l.delete_wake(T0 + 2).unwrap();
        l.set_wake(
            waking(serde_json::json!({ "url": "https://agents.example.com/v2", "debounce": "1m" })),
            T0 + 3,
        )
        .unwrap();
        assert_eq!(wake_tasks(&db), 1, "the old one is still in flight");
        l.finish(&old, a.fence, http(200), T0 + 4, 0.0).unwrap();
        let next = wake_id(&l).expect("t2 still needs a wake");
        assert_ne!(next, old);
        assert_eq!(l.task(&next).unwrap().url, "https://agents.example.com/v2");
    }

    #[test]
    fn a_wake_cannot_be_redriven_and_forgets_its_token() {
        let db = Memory::new();
        let l = setup(&db);
        l.set_wake(waking(serde_json::json!({})), T0).unwrap();
        finish_to_inbox(&l, "t1", T0);
        let id = wake_id(&l).unwrap();
        send_wake(&l, 404, T0);
        assert_eq!(
            l.redrive(&id, T0 + 1).unwrap_err().code,
            "wake_not_redrivable"
        );
        let stored: String =
            db.0.query_row("SELECT headers FROM tasks WHERE id = ?", [&id], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(!stored.contains("t0ken"), "{stored}");
        assert_eq!(l.task(&id).unwrap().header_names, ["Authorization"]);
    }

    #[test]
    fn a_wake_never_spends_attempts_promised_to_waiting_tasks() {
        let db = Memory::new();
        let l = setup(&db);
        db.0.execute("UPDATE project SET attempt_budget = 3", [])
            .unwrap();
        l.set_wake(waking(serde_json::json!({})), T0).unwrap();
        let later = ScheduleRequest {
            delay: Some("1h".into()),
            ..req("https://a.example.com/")
        };
        l.schedule(later, "t2", T0).unwrap();
        finish_to_inbox(&l, "t1", T0);
        // One attempt is spare: the wake may use it, and fails retryably.
        send_wake(&l, 503, T0);
        let id = wake_id(&l).unwrap();
        let retry = l.task(&id).unwrap().next_attempt_ms.unwrap();
        let d = l
            .publish_due(P, retry)
            .unwrap()
            .into_iter()
            .find(|d| d.task_id == id)
            .unwrap();
        // The last attempt is t2's, so the wake stands down instead of retrying.
        assert_eq!(
            l.claim(&d.task_id, d.generation, retry).unwrap(),
            Claimed::Skip("budget_reserved")
        );
        assert_eq!(l.task(&id).unwrap().status, Status::Canceled);
        assert_eq!(
            l.project().unwrap().wake.unwrap().consecutive_failures,
            0,
            "not a failure"
        );
        let t2 = T0 + 3_600_000;
        answer(&l, "t2", 200, "", &[], t2);
        assert_eq!(l.task("t2").unwrap().status, Status::Succeeded);
    }

    #[test]
    fn the_debounce_counts_from_the_last_wake_sent() {
        let db = Memory::new();
        let l = setup(&db);
        let daily = || waking(serde_json::json!({ "debounce": "24h" }));
        l.set_wake(daily(), T0).unwrap();
        finish_to_inbox(&l, "t1", T0);
        send_wake(&l, 200, T0);
        finish_to_inbox(&l, "t2", T0 + 1);
        assert_eq!(wake_due(&l), Some(T0 + DAY));
        // Replacing the setting (say, a new token) does not push the pending wake out.
        l.set_wake(daily(), T0 + 3_600_000).unwrap();
        assert_eq!(wake_due(&l), Some(T0 + DAY));
        // A wake dropped because everything was read does not count as sent.
        l.read_inbox(None, None, true).unwrap();
        let id = wake_id(&l).unwrap();
        let d = l
            .publish_due(P, T0 + DAY)
            .unwrap()
            .into_iter()
            .find(|d| d.task_id == id)
            .unwrap();
        assert_eq!(
            l.claim(&d.task_id, d.generation, T0 + DAY).unwrap(),
            Claimed::Skip("inbox_read")
        );
        finish_to_inbox(&l, "t3", T0 + DAY + 3_600_000);
        assert_eq!(
            wake_due(&l),
            Some(T0 + DAY + 3_600_000),
            "a day after the last one sent"
        );
    }

    // -- crons that report ----------------------------------------------------------------

    fn reporting(name: &str, extra: serde_json::Value) -> CronRequest {
        let mut r = serde_json::json!({
            "name": name,
            "schedule": "0 6 * * *",
            "url": "https://a.example.com/status",
            "method": "GET",
            "max_attempts": 1,
            "callback_url": "inbox",
            "capture": true,
        });
        r.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(r).unwrap()
    }

    fn run_id(l: &Ledger<'_, Memory>, name: &str) -> String {
        l.cron(name).unwrap().last_run_task_id.unwrap()
    }

    #[test]
    fn a_cron_run_reports_to_the_inbox_with_its_response() {
        let db = Memory::new();
        let l = setup(&db);
        let (c, _) = l
            .put_cron(reporting("health", serde_json::json!({})), T0)
            .unwrap();
        assert_eq!(
            (c.callback_url.as_deref(), c.report, c.capture),
            (Some("inbox"), Report::Always, true)
        );
        l.fire_crons(SIX).unwrap();
        let id = run_id(&l, "health");
        let t = l.task(&id).unwrap();
        assert_eq!(
            (t.callback_url.as_deref(), t.capture),
            (Some("inbox"), true)
        );
        answer(&l, &id, 200, r#"{"deploy":"failed"}"#, &[], SIX);
        let e = &inbox(&l).events[0];
        assert_eq!(e["event"], "task.finished");
        assert_eq!(e["task"]["cron"], "health");
        assert_eq!(e["task"]["response"]["body"], r#"{"deploy":"failed"}"#);
        // The next day's run reports again.
        l.fire_crons(SIX + DAY).unwrap();
        answer(&l, &run_id(&l, "health"), 200, "{}", &[], SIX + DAY);
        assert_eq!(inbox(&l).events.len(), 2);
    }

    #[test]
    fn report_failures_keeps_a_healthy_cron_quiet() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(
            reporting("health", serde_json::json!({ "report": "failures" })),
            T0,
        )
        .unwrap();
        l.set_wake(waking(serde_json::json!({})), T0).unwrap();
        l.fire_crons(SIX).unwrap();
        answer(&l, &run_id(&l, "health"), 200, "fine", &[], SIX);
        assert_eq!(inbox(&l).events.len(), 0, "a success says nothing");
        assert_eq!(wake_tasks(&db), 0, "and wakes nobody");

        l.fire_crons(SIX + DAY).unwrap();
        let failed = run_id(&l, "health");
        answer(&l, &failed, 500, "broken", &[], SIX + DAY);
        let e = &inbox(&l).events[0];
        assert_eq!(
            (e["task"]["id"].as_str(), e["task"]["status"].as_str()),
            (Some(failed.as_str()), Some("failed"))
        );
        assert_eq!(e["task"]["response"]["body"], "broken");
        assert_eq!(wake_tasks(&db), 1, "the failure wakes the agent");
    }

    #[test]
    fn a_cron_reporting_to_a_url_reserves_its_callback() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(
            reporting(
                "health",
                serde_json::json!({ "callback_url": "https://cb.example.com/runs" }),
            ),
            T0,
        )
        .unwrap();
        db.0.execute("UPDATE project SET attempt_budget = 1", [])
            .unwrap();
        l.fire_crons(SIX).unwrap();
        let c = l.cron("health").unwrap();
        assert_eq!(
            (c.last_run_task_id, c.health_reason.as_deref()),
            (None, Some("budget_exhausted")),
            "a run and its callback need two attempts"
        );

        db.0.execute("UPDATE project SET attempt_budget = 10", [])
            .unwrap();
        l.fire_crons(SIX + DAY).unwrap();
        let id = run_id(&l, "health");
        answer(&l, &id, 200, "ok", &[], SIX + DAY);
        let cb = l
            .task(&id)
            .unwrap()
            .callback_task_id
            .expect("a callback task");
        assert_eq!(l.task(&cb).unwrap().url, "https://cb.example.com/runs");
    }

    #[test]
    fn replacing_a_cron_changes_how_it_reports_and_old_crons_stay_quiet() {
        let db = Memory::new();
        let l = setup(&db);
        let (c, _) = l.put_cron(daily("backup"), T0).unwrap();
        assert_eq!(
            (c.callback_url, c.report, c.capture),
            (None, Report::Always, false)
        );
        let (c, created) = l
            .put_cron(
                reporting("backup", serde_json::json!({ "report": "failures" })),
                T0,
            )
            .unwrap();
        assert!(!created);
        assert_eq!(
            (c.callback_url.as_deref(), c.report),
            (Some("inbox"), Report::Failures)
        );
        let e = l
            .put_cron(
                CronRequest {
                    report: Some(Report::Failures),
                    ..daily("x")
                },
                T0,
            )
            .unwrap_err();
        assert_eq!((e.status, e.field), (400, Some("report")));
    }

    #[test]
    fn a_failures_only_run_reports_when_abandoned_but_not_when_canceled() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(
            reporting("health", serde_json::json!({ "report": "failures" })),
            T0,
        )
        .unwrap();
        l.fire_crons(SIX).unwrap();
        let canceled = run_id(&l, "health");
        let t = l.cancel(&canceled, SIX).unwrap();
        assert!(t.report_failures_only);
        assert_eq!(inbox(&l).events.len(), 0, "a canceled run did not fail");

        // Started, then out of attempts before it could be sent: that run failed, so it reports.
        l.fire_crons(SIX + DAY).unwrap();
        let starved = run_id(&l, "health");
        db.0.execute("UPDATE project SET attempts_used = attempt_budget", [])
            .unwrap();
        let d = l
            .publish_due(P, SIX + DAY)
            .unwrap()
            .into_iter()
            .find(|d| d.task_id == starved)
            .unwrap();
        assert_eq!(
            l.claim(&d.task_id, d.generation, SIX + DAY).unwrap(),
            Claimed::Skip("budget_exhausted")
        );
        let e = &inbox(&l).events[0];
        assert_eq!(
            (e["task"]["id"].as_str(), e["task"]["status"].as_str()),
            (Some(starved.as_str()), Some("failed"))
        );
    }

    // -- secrets --------------------------------------------------------------------------

    fn hosts(h: &[&str]) -> Vec<String> {
        h.iter().map(|s| s.to_string()).collect()
    }

    fn with_secret_header(url: &str, value: &str) -> ScheduleRequest {
        let mut r = req(url);
        r.headers.insert("Authorization".into(), value.into());
        r
    }

    #[test]
    fn secrets_are_listed_without_values_and_capped() {
        let db = Memory::new();
        let l = setup(&db);
        let (v, created) = l
            .put_secret("GH", "v1:sealed-a", 20, &hosts(&["api.github.com"]), T0)
            .unwrap();
        assert!(created);
        assert_eq!(
            (v.name.as_str(), v.allowed_hosts.clone()),
            ("GH", hosts(&["api.github.com"]))
        );
        let (v, created) = l
            .put_secret("GH", "v1:sealed-b", 20, &hosts(&["*.github.com"]), T0 + 5)
            .unwrap();
        assert!(!created);
        assert_eq!((v.created_ms, v.updated_ms), (T0, T0 + 5));
        let listed = serde_json::to_string(&l.secrets().unwrap()).unwrap();
        assert!(!listed.contains("sealed"), "{listed}");
        for i in 1..MAX_SECRETS {
            l.put_secret(&format!("S{i}"), "v1:x", 20, &hosts(&["a.example.com"]), T0)
                .unwrap();
        }
        let e = l
            .put_secret("ONE_MORE", "v1:x", 20, &hosts(&["a.example.com"]), T0)
            .unwrap_err();
        assert_eq!((e.status, e.code), (429, "secret_limit"));
        l.put_secret("GH", "v1:sealed-c", 20, &hosts(&["api.github.com"]), T0)
            .unwrap();
        assert_eq!(l.delete_secret("GH").unwrap().name, "GH");
        assert_eq!(l.delete_secret("GH").unwrap_err().code, "secret_not_found");
    }

    #[test]
    fn new_work_may_only_send_a_secret_to_its_hosts() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_secret("GH", "v1:sealed", 20, &hosts(&["api.github.com"]), T0)
            .unwrap();
        let ok = with_secret_header("https://api.github.com/user", "Bearer {{secret:GH}}");
        let (t, _) = l.schedule(ok, "t1", T0).unwrap();
        assert_eq!(t.header_names, ["Authorization"]);

        let e = l
            .schedule(
                with_secret_header("https://evil.example.com/", "{{secret:GH}}"),
                "t2",
                T0,
            )
            .unwrap_err();
        assert_eq!(
            (e.status, e.code, e.field),
            (400, "secret_host_not_allowed", Some("url"))
        );
        let e = l
            .schedule(
                with_secret_header("https://api.github.com/", "{{secret:NOPE}}"),
                "t3",
                T0,
            )
            .unwrap_err();
        assert_eq!((e.code, e.field), ("secret_not_found", Some("headers")));
        let e = l
            .schedule(
                with_secret_header("https://api.github.com/", "{{secret:GH"),
                "t4",
                T0,
            )
            .unwrap_err();
        assert_eq!(e.field, Some("headers"));
        let mut in_body = req("https://api.github.com/");
        in_body.body = Some("{{secret:GH}}".into());
        assert_eq!(
            l.schedule(in_body, "t5", T0).unwrap_err().field,
            Some("body")
        );

        // Crons, watches and wakes are checked the same way.
        let mut cron = daily("c");
        cron.url = "https://evil.example.com/".into();
        cron.headers
            .insert("Authorization".into(), "{{secret:GH}}".into());
        assert_eq!(
            l.put_cron(cron, T0).unwrap_err().code,
            "secret_host_not_allowed"
        );
        let w = watching(serde_json::json!({ "headers": { "Authorization": "{{secret:GH}}" } }));
        assert_eq!(
            l.watch(w, "w1", T0).unwrap_err().code,
            "secret_host_not_allowed"
        );
        let wake = waking(serde_json::json!({ "headers": { "Authorization": "{{secret:GH}}" } }));
        assert_eq!(
            l.set_wake(wake, T0).unwrap_err().code,
            "secret_host_not_allowed"
        );
    }

    #[test]
    fn the_consumer_gets_the_sealed_secret_and_the_reference() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_secret("GH", "v1:sealed", 20, &hosts(&["api.github.com"]), T0)
            .unwrap();
        l.schedule(
            with_secret_header("https://api.github.com/user", "Bearer {{secret:GH}}"),
            "t1",
            T0,
        )
        .unwrap();
        let d = l.publish_due(P, T0).unwrap().remove(0);
        let a = deliver(&l, &d, T0);
        assert_eq!(
            a.headers,
            [(
                "Authorization".to_string(),
                "Bearer {{secret:GH}}".to_string()
            )],
            "the reference, filled in by the consumer"
        );
        assert_eq!(
            a.secrets,
            [SealedSecret {
                name: "GH".into(),
                sealed: "v1:sealed".into()
            }]
        );
        let stored: String =
            db.0.query_row("SELECT headers FROM tasks WHERE id = 't1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(stored.contains("{{secret:GH}}"));
    }

    #[test]
    fn a_task_whose_secret_is_gone_or_narrowed_fails_without_an_attempt() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_secret("GH", "v1:sealed", 20, &hosts(&["api.github.com"]), T0)
            .unwrap();
        let mut r = with_secret_header("https://api.github.com/user", "Bearer {{secret:GH}}");
        r.callback_url = Some(INBOX.into());
        l.schedule(r, "gone", T0).unwrap();
        l.schedule(
            with_secret_header("https://api.github.com/x", "{{secret:GH}}"),
            "narrowed",
            T0,
        )
        .unwrap();
        l.delete_secret("GH").unwrap();
        let used = l.project().unwrap().attempts_used;
        let due = l.publish_due(P, T0).unwrap();
        let find = |id: &str| due.iter().find(|d| d.task_id == id).unwrap().clone();
        let d = find("gone");
        assert_eq!(
            l.claim(&d.task_id, d.generation, T0).unwrap(),
            Claimed::Skip("secret_missing")
        );
        let t = l.task("gone").unwrap();
        assert_eq!(
            (t.status, t.failure.as_deref(), t.attempt_count),
            (Status::Failed, Some("secret_missing"), 0)
        );
        assert_eq!(l.project().unwrap().attempts_used, used, "costs nothing");
        assert_eq!(
            inbox(&l).events[0]["task"]["failure"],
            "secret_missing",
            "and it reports"
        );

        // Put back, but for other hosts only.
        l.put_secret("GH", "v1:sealed", 20, &hosts(&["uploads.github.com"]), T0)
            .unwrap();
        let d = find("narrowed");
        assert_eq!(
            l.claim(&d.task_id, d.generation, T0).unwrap(),
            Claimed::Skip("secret_host_not_allowed")
        );
        // Fixing the secret and redriving sends it, and clears the failure.
        l.put_secret("GH", "v1:sealed", 20, &hosts(&["api.github.com"]), T0)
            .unwrap();
        let t = l.redrive("gone", T0 + 1).unwrap();
        assert_eq!((t.status, t.failure), (Status::Scheduled, None));
        let d = l
            .publish_due(P, T0 + 1)
            .unwrap()
            .into_iter()
            .find(|d| d.task_id == "gone")
            .unwrap();
        assert_eq!(deliver(&l, &d, T0 + 1).secrets.len(), 1);
    }

    #[test]
    fn headers_are_sized_with_their_secrets_filled_in() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_secret("BIG", "v1:sealed", 4_000, &hosts(&["api.github.com"]), T0)
            .unwrap();
        let mut r = req("https://api.github.com/");
        r.headers.insert("A".into(), "{{secret:BIG}}".into());
        r.headers.insert("B".into(), "{{secret:BIG}}".into());
        r.headers.insert("C".into(), "{{secret:BIG}}".into());
        let e = l.schedule(r, "t1", T0).unwrap_err();
        assert_eq!((e.status, e.field), (400, Some("headers")));

        // Accepted at its size, then the secret is replaced by a longer one.
        l.put_secret("SMALL", "v1:sealed", 20, &hosts(&["api.github.com"]), T0)
            .unwrap();
        let mut r = req("https://api.github.com/");
        r.headers.insert("A".into(), "{{secret:SMALL}}".into());
        r.headers.insert("B".into(), "x".repeat(6_000));
        l.schedule(r, "t2", T0).unwrap();
        l.put_secret("SMALL", "v1:sealed", 4_000, &hosts(&["api.github.com"]), T0)
            .unwrap();
        let d = l.publish_due(P, T0).unwrap().remove(0);
        assert_eq!(
            l.claim(&d.task_id, d.generation, T0).unwrap(),
            Claimed::Skip("secret_too_large")
        );
    }

    #[test]
    fn without_the_key_a_task_needing_a_secret_fails_before_its_attempt() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_secret("GH", "v1:sealed", 20, &hosts(&["api.github.com"]), T0)
            .unwrap();
        l.schedule(
            with_secret_header("https://api.github.com/", "{{secret:GH}}"),
            "t1",
            T0,
        )
        .unwrap();
        l.schedule(req("https://api.github.com/plain"), "plain", T0)
            .unwrap();
        let l = Ledger::open(&db).unwrap().without_secrets();
        let due = l.publish_due(P, T0).unwrap();
        let find = |id: &str| due.iter().find(|d| d.task_id == id).unwrap().clone();
        let used = l.project().unwrap().attempts_used;
        let d = find("t1");
        assert_eq!(
            l.claim(&d.task_id, d.generation, T0).unwrap(),
            Claimed::Skip("secret_unavailable")
        );
        assert_eq!(
            l.task("t1").unwrap().failure.as_deref(),
            Some("secret_unavailable")
        );
        assert_eq!(l.project().unwrap().attempts_used, used);
        let d = find("plain");
        assert!(
            matches!(
                l.claim(&d.task_id, d.generation, T0).unwrap(),
                Claimed::Deliver(_)
            ),
            "others still go"
        );
    }

    #[test]
    fn a_watch_whose_secret_is_gone_says_why() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_secret("API", "v1:sealed", 20, &hosts(&["api.example.com"]), T0)
            .unwrap();
        l.watch(
            watching(serde_json::json!({ "headers": { "Authorization": "{{secret:API}}" } })),
            "w1",
            T0,
        )
        .unwrap();
        l.delete_secret("API").unwrap();
        let d = l.publish_due(P, T0).unwrap().remove(0);
        assert_eq!(
            l.claim(&d.task_id, d.generation, T0).unwrap(),
            Claimed::Skip("secret_missing")
        );
        let w = l.task("w1").unwrap();
        assert_eq!(
            (w.status, w.watch.unwrap().reason.as_deref()),
            (Status::Failed, Some("secret_missing"))
        );
    }
}
