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
    DEFAULT_MAX_ATTEMPTS, MAX_CREDIT_ATTEMPTS, MAX_CRONS, MAX_HEADER_BYTES, MAX_RETAINED_TASKS,
    MAX_RETAINED_TASKS_PAID, MAX_SECRETS, RETENTION_MS, TRIAL_ATTEMPTS, TRIAL_DURATION_MS,
    WATCH_EXCERPT_BYTES,
};
use runlater_core::{
    fill, header_references, host_allowed, judge, next_poll, valid_lane, Attempt, CancelError,
    ClaimError, CronRequest, FieldError, Method, Outcome, Poll, PollBody, RedriveError, Report,
    Schedule, ScheduleRequest, Status, Task, Until, WakeRequest, Watch, WatchRequest, INBOX,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

mod budget;
mod crons;
mod hooks;
mod inbox;
mod secrets;
mod tasks;
mod watches;
use budget::{verdict, Cost, Standing, Verdict};
pub use crons::{CronList, CronView};
pub use hooks::{
    token_hash, CreatedHook, HookDelivery, HookList, HookOutcome, HookReply, HookRow, HookView,
    Received,
};
pub use inbox::{InboxCursor, InboxPage, WakeView};
pub use runlater_core::Health;
pub use secrets::{ResealCount, SealedSecret, Sealer, SecretList, SecretView};
use tasks::{Kind, NewTask, Reporting};
pub use watches::{OnMatchView, WatchView};

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
    // 10: watches that act. `on_match` is the request a watch sends when it matches;
    // `action_reserve` the rows and attempts admission set aside for it (the action, and its
    // callback); `triggered_by` names the watch on the task it sent.
    &[
        "ALTER TABLE tasks ADD COLUMN on_match TEXT",
        "ALTER TABLE tasks ADD COLUMN action_reserve INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE tasks ADD COLUMN triggered_by TEXT",
    ],
    // 11: inbound hooks. A hook is found by the SHA-256 of its token; `sealed_secret` is its
    // verify secret, sealed by the API. `hook_deliveries` holds senders' delivery ids for a day,
    // to drop retries; `hook_day`/`hook_day_count` cap the events a project gets per UTC day.
    &[
        "CREATE TABLE IF NOT EXISTS hooks (
            name TEXT PRIMARY KEY,
            token_hash TEXT NOT NULL UNIQUE,
            verify TEXT,
            sealed_secret TEXT,
            keep_headers TEXT NOT NULL,
            created_ms INTEGER NOT NULL,
            last_received_ms INTEGER,
            received INTEGER NOT NULL DEFAULT 0,
            refused TEXT NOT NULL DEFAULT '{}',
            rate_tat_ms INTEGER NOT NULL DEFAULT 0,
            day INTEGER NOT NULL DEFAULT 0,
            day_count INTEGER NOT NULL DEFAULT 0
        )",
        "CREATE TABLE IF NOT EXISTS hook_deliveries (
            hook TEXT NOT NULL,
            delivery_id TEXT NOT NULL,
            expires_ms INTEGER NOT NULL,
            PRIMARY KEY (hook, delivery_id)
        )",
        "ALTER TABLE project ADD COLUMN hook_day INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE project ADD COLUMN hook_day_count INTEGER NOT NULL DEFAULT 0",
    ],
    // 12: hook filters. `accept` holds the rules a request must match to be kept; `filtered`
    // counts the requests that matched none, answered and dropped.
    &[
        "ALTER TABLE hooks ADD COLUMN accept TEXT",
        "ALTER TABLE hooks ADD COLUMN filtered INTEGER NOT NULL DEFAULT 0",
    ],
];

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

    /// A request that failed validation, naming the field at fault.
    pub fn invalid(e: impl FieldError) -> Self {
        Self::bad_request(e.to_string()).in_field(e.field())
    }

    /// The same error, naming the request field at fault.
    pub fn in_field(self, field: &'static str) -> Self {
        Self {
            field: Some(field),
            ..self
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
    /// Where AgentLater sends a wake when the inbox gets something; see `set_wake`.
    #[serde(default)]
    pub wake: Option<WakeView>,
}

/// What a [`Ledger::tick`] leaves its caller: tasks to put on the queue, already marked
/// published, and the rows it could not read (failed where they stood).
#[derive(Debug, Default)]
pub struct Tick {
    pub dispatches: Vec<Dispatch>,
    pub unreadable: Vec<DbError>,
}

/// What `recover` did: how many expired leases it settled, and the rows it could not read
/// (failed where they stood).
#[derive(Debug, Default)]
struct Recovery {
    recovered: usize,
    unreadable: Vec<DbError>,
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
    /// How long after its scheduled time this attempt started, when that says something about
    /// AgentLater: only a first attempt at the task's original time. A task waiting its turn in a
    /// lane, a wake held back on purpose, a retry and a redrive are late by design.
    #[serde(default)]
    pub late_ms: Option<i64>,
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
    on_match: Option<String>,
    triggered_by: Option<String>,
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

const TASK_COLUMNS: &str = "seq, id, status, due_ms, max_attempts, run_attempts, \
    total_attempts, generation, fence, lease_until_ms, cancel_requested, terminal_ms, \
    method, url, headers, body, run_at_ms, idempotency_key, callback_url, callback_task_id, \
    callback_for, lane, cron, alert_for, watch, watch_errors, watch_state, capture, response, \
    inbox_seq, wake_call, report_failures_only, failure, on_match, triggered_by, created_ms, \
    updated_ms";

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

// ---------------------------------------------------------------------------------------

pub struct Ledger<'a, D: Db> {
    db: &'a D,
    /// The keys secrets are sealed with. Without them nothing can be sealed, and work that
    /// needs a secret fails before it is attempted rather than spending an attempt it can't make.
    sealer: Option<Sealer>,
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
        Ok(Self { db, sealer: None })
    }

    /// The ledger for a deployment that has keys to seal secrets with.
    pub fn with_sealer(self, sealer: Option<Sealer>) -> Self {
        Self { sealer, ..self }
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
        view.wake = self.wake_view()?;
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
            return Err(
                ApiError::bad_request("payment_id is not a valid payment id")
                    .in_field("payment_id"),
            );
        }
        if !(1..=MAX_CREDIT_ATTEMPTS).contains(&attempts) {
            return Err(
                ApiError::bad_request(format!("attempts must be 1-{MAX_CREDIT_ATTEMPTS}"))
                    .in_field("attempts"),
            );
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
        // which costs AgentLater a little; the other order could leave a paid pack never credited.
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
        self.restart_stopped_crons(now)?;
        Ok((self.project()?, true))
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
    fn publish_due(&self, project_id: &str, now: i64) -> Result<Vec<Dispatch>, DbError> {
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
        let kind = row.kind()?;
        let reporting = row.reporting();

        let current = task.generation == generation && task.is_due(now);
        let standing = Standing {
            can_attempt: project.can_attempt(now),
            left: if now < project.expires_ms {
                project.attempt_budget - project.attempts_used
            } else {
                0
            },
            inbox_unread: project.inbox_unread,
        };
        match verdict(kind, &task, current, standing, || self.reserved_attempts())? {
            Verdict::Spend => {}
            Verdict::LastTry => task.max_attempts = task.run_attempts + 1,
            Verdict::StandDown(why) => {
                if task.cancel(now).is_ok() {
                    self.save_row(&row, &task, now)?;
                }
                return Ok(Claimed::Skip(why));
            }
            Verdict::Exhausted => {
                if current && task.abandon(now) {
                    self.watch_gave_up(&row, kind, "budget_exhausted")?;
                    self.save_row(&row, &task, now)?;
                    return Ok(Claimed::Skip("budget_exhausted"));
                }
                return Ok(Claimed::Skip("stale"));
            }
        }

        if kind.lane().is_some()
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
                    self.watch_gave_up(&row, kind, why)?;
                    self.save_row(&row, &task, now)?;
                    return Ok(Claimed::Skip(why));
                }
                return Ok(Claimed::Skip("stale"));
            }
        };
        // Every retry, repoll, abandon and redrive moves the generation on.
        let on_schedule = task.generation == 1 && kind.measures_lateness();
        let attempt: Attempt = match task.claim(generation, now) {
            Ok(a) => a,
            Err(ClaimError::Stale) => return Ok(Claimed::Skip("stale")),
            Err(ClaimError::NotDue { .. }) => return Ok(Claimed::Skip("not_due")),
        };
        self.charge()?;
        self.db.exec(
            "INSERT INTO attempts (task_id, number, started_ms) VALUES (?, ?, ?)",
            vec![task_id.into(), attempt.number.into(), now.into()],
        )?;
        self.save_row(&row, &task, now)?;
        // A wake says how things stand when it is sent, and covers every event so far.
        let body = if kind.is_wake() {
            Some(self.sending_wake(&project, now)?)
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
            read_body: kind.is_watch() || reporting.capture,
            capture: reporting.capture,
            secrets,
            late_ms: (on_schedule && attempt.number == 1).then(|| (now - row.run_at_ms).max(0)),
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
        if row.kind()?.is_watch() {
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
        if let (true, Outcome::Http { status, .. }) = (row.reporting().capture, &outcome) {
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

    /// Settles attempts whose consumer vanished. A row that can't be read is failed on its
    /// own, so it can't hold up the others (or wake the ledger again every time).
    fn recover(&self, now: i64, mut jitter: impl FnMut() -> f64) -> Result<Recovery, DbError> {
        let rows: Vec<TaskRow> = self.db.query(
            &format!(
                "SELECT {TASK_COLUMNS} FROM tasks \
                 WHERE status = 'delivering' AND lease_until_ms <= ?"
            ),
            vec![now.into()],
        )?;
        let mut recovery = Recovery::default();
        for row in &rows {
            let (mut task, kind) = match row.task().and_then(|t| Ok((t, row.kind()?))) {
                Ok(read) => read,
                Err(e) => {
                    recovery.unreadable.push(e);
                    if let Err(e) = self.fail_unreadable(row, now) {
                        recovery.unreadable.push(e);
                    }
                    continue;
                }
            };
            recovery.recovered += 1;
            let fence = task.fence;
            if kind.is_watch() {
                self.finish_poll(row, fence, Outcome::LeaseExpired, None, now)?;
                continue;
            }
            if task.expire_lease(now, jitter()).is_some() {
                self.record_outcome(&row.id, fence, &Outcome::LeaseExpired, now)?;
                self.save_row(row, &task, now)?;
            }
        }
        Ok(recovery)
    }

    /// Fails a delivering row whose columns don't make a task (`failure: unreadable`). Its
    /// attempt is closed and its report sent, as for any failure; what its kind would set off
    /// (a cron's health, a wake's count, a watch's action) can't run without the kind.
    /// The row leaves `delivering` first, so a later step failing can't keep it there.
    fn fail_unreadable(&self, row: &TaskRow, now: i64) -> Result<(), DbError> {
        self.db.exec(
            "UPDATE tasks SET status = ?, lease_until_ms = NULL, terminal_ms = ?, \
             failure = 'unreadable', updated_ms = ? WHERE id = ?",
            vec![
                text(&Status::Failed).into(),
                now.into(),
                now.into(),
                row.id.as_str().into(),
            ],
        )?;
        self.record_outcome(&row.id, row.fence, &Outcome::LeaseExpired, now)?;
        match row.reporting().to {
            Some(INBOX) => self.report_to_inbox(&row.id, now),
            Some(url) => self.queue_callback(&row.id, url, row.generation, now),
            None => Ok(()),
        }
    }

    /// Deletes tasks whose retention period has passed, captured responses a week after
    /// their task finished, and old inbox events. Returns how many tasks.
    fn purge(&self, now: i64) -> Result<usize, DbError> {
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

    /// Everything the ledger does on its own once time has passed, in the order it must happen:
    /// settle lost attempts, purge what is past retention, start due cron runs, then publish
    /// what is due (so a run started here is dispatched here). The caller queues the
    /// dispatches and sets the alarm to [`Self::next_wake`].
    pub fn tick(&self, now: i64, jitter: impl FnMut() -> f64) -> Result<Tick, DbError> {
        let unreadable = self.recover(now, jitter)?.unreadable;
        self.purge(now)?;
        self.fire_crons(now)?;
        let dispatches = match self.project_row()? {
            Some(project) => self.publish_due(&project.id, now)?,
            None => vec![],
        };
        Ok(Tick {
            dispatches,
            unreadable,
        })
    }

    /// When the alarm should next run, if there is anything left to do.
    pub fn next_wake(&self) -> Result<Option<i64>, DbError> {
        let rows: Vec<Due> = self.db.query(
            &format!(
                "SELECT MIN(t) AS t FROM ( \
                 SELECT CASE WHEN published_ms IS NOT NULL AND published_ms + {REPUBLISH_MS} > due_ms \
                        THEN published_ms + {REPUBLISH_MS} ELSE due_ms END AS t \
                   FROM tasks t WHERE status IN ('scheduled', 'retrying') AND {LANE_CLEAR} \
                 UNION ALL SELECT lease_until_ms FROM tasks WHERE status = 'delivering' \
                 UNION ALL SELECT terminal_ms + {RETENTION_MS} FROM tasks WHERE terminal_ms IS NOT NULL)"
            ),
            vec![],
        )?;
        // Separate queries: Durable Object SQLite allows at most five terms in a compound SELECT.
        let captures: Vec<Due> = self.db.query(
            &format!(
                "SELECT MIN(terminal_ms) + {CAPTURE_RETENTION_MS} AS t FROM tasks \
                 WHERE response IS NOT NULL AND terminal_ms IS NOT NULL"
            ),
            vec![],
        )?;
        let inbox = self.inbox_next_due()?;
        let crons = self.crons_next_due()?;
        Ok(rows
            .iter()
            .chain(&captures)
            .filter_map(|w| w.t)
            .chain(inbox)
            .chain(crons)
            .min())
    }

    // -- helpers --------------------------------------------------------------------------

    fn lane_clear(&self, task_id: &str) -> Result<bool, DbError> {
        let rows: Vec<IdRow> = self.db.query(
            &format!("SELECT id FROM tasks t WHERE id = ? AND {LANE_CLEAR}"),
            vec![task_id.into()],
        )?;
        Ok(!rows.is_empty())
    }

    /// The inbox's version of a callback: the same report, as an event, in this transaction.
    fn report_to_inbox(&self, task_id: &str, now: i64) -> Result<(), DbError> {
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

#[derive(Deserialize)]
struct SeqRow {
    seq: i64,
}

/// An earliest due time, from a `SELECT MIN(t) AS t`.
#[derive(Deserialize)]
struct Due {
    t: Option<i64>,
}

#[cfg(test)]
mod testing;

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::db::sqlite::Memory;
    use runlater_core::limits::LEASE_MS;

    #[test]
    fn derived_ids_are_the_start_of_a_sha256() {
        assert_eq!(derived_id("abc"), "task_ba7816bf8f01cfea4141");
    }

    #[test]
    fn a_first_attempt_reports_how_late_it_started() {
        let db = Memory::new();
        let l = setup(&db);
        let (t, _) = l
            .schedule(req("https://api.stripe.com/"), "t1", T0)
            .unwrap();
        let d = l.publish_due(P, T0 + 40).unwrap().remove(0);
        let first = deliver(&l, &d, T0 + 40);
        assert_eq!(first.late_ms, Some(40));

        l.finish(&t.id, first.fence, http(500), T0 + 50, 0.0)
            .unwrap();
        let due = l.task(&t.id).unwrap().next_attempt_ms.unwrap();
        let d = l.publish_due(P, due).unwrap().remove(0);
        assert_eq!(
            deliver(&l, &d, due + 5).late_ms,
            None,
            "a retry is late by design"
        );
    }

    #[test]
    fn a_redrive_or_a_lane_does_not_count_as_late() {
        let db = Memory::new();
        let l = setup(&db);
        let once = ScheduleRequest {
            max_attempts: Some(1),
            ..req("https://api.stripe.com/")
        };
        let (t, _) = l.schedule(once, "t1", T0).unwrap();
        let d = l.publish_due(P, T0).unwrap().remove(0);
        let first = deliver(&l, &d, T0);
        l.finish(&t.id, first.fence, http(500), T0 + 1, 0.0)
            .unwrap();
        l.redrive(&t.id, T0 + 1_000).unwrap();
        let d = l.publish_due(P, T0 + 1_000).unwrap().remove(0);
        assert_eq!(deliver(&l, &d, T0 + 1_000).late_ms, None);

        let db = Memory::new();
        let l = setup(&db);
        l.schedule(in_lane("a", T0), "t1", T0).unwrap();
        let d = l.publish_due(P, T0 + 40).unwrap().remove(0);
        assert_eq!(deliver(&l, &d, T0 + 40).late_ms, None);
    }

    #[test]
    fn a_task_abandoned_before_its_first_attempt_is_not_late_when_redriven() {
        let db = Memory::new();
        let l = setup(&db);
        let (t, _) = l
            .schedule(req("https://api.stripe.com/"), "t1", T0)
            .unwrap();
        let d = l.publish_due(P, T0).unwrap().remove(0);
        db.0.execute("UPDATE project SET attempts_used = attempt_budget", [])
            .unwrap();
        assert!(matches!(
            l.claim(&d.task_id, d.generation, T0 + 1).unwrap(),
            Claimed::Skip("budget_exhausted")
        ));
        db.0.execute("UPDATE project SET attempts_used = 0", [])
            .unwrap();
        l.redrive(&t.id, T0 + 1_000).unwrap();
        let d = l.publish_due(P, T0 + 1_000).unwrap().remove(0);
        let a = deliver(&l, &d, T0 + 1_000);
        assert_eq!(a.attempt, 1);
        assert_eq!(a.late_ms, None);
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
            i64::from(TRIAL_ATTEMPTS) - 30 + 50_000,
            "unused trial attempts carry over"
        );
        assert_eq!(p.expires_ms, T0 + 10 + CREDIT_VALIDITY_MS);

        let (again, added) = l.add_credit("cs_test_1", 50_000, T0 + 20).unwrap();
        assert!(!added);
        assert_eq!(again, p, "a repeated payment must not add twice");

        let (two, _) = l.add_credit("cs_test_2", 50_000, T0 + 30).unwrap();
        assert_eq!(
            two.attempts_remaining,
            i64::from(TRIAL_ATTEMPTS) - 30 + 100_000
        );
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

        assert_eq!(l.recover(T0 + LEASE_MS - 1, || 0.0).unwrap().recovered, 0);
        assert_eq!(l.recover(T0 + LEASE_MS, || 0.0).unwrap().recovered, 1);
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
    fn an_unreadable_row_does_not_hold_up_recovery() {
        let db = Memory::new();
        let l = setup(&db);
        let reported = ScheduleRequest {
            callback_url: Some(INBOX.into()),
            ..req("https://a.example.com/")
        };
        l.schedule(reported, "t1", T0).unwrap();
        l.schedule(req("https://a.example.com/"), "t2", T0).unwrap();
        for d in l.publish_due(P, T0).unwrap() {
            deliver(&l, &d, T0);
        }
        // A row that names both a cron alert and a wake: two kinds at once.
        db.exec(
            "UPDATE tasks SET alert_for = 'c', wake_call = 1 WHERE id = 't1'",
            vec![],
        )
        .unwrap();

        let recovery = l.recover(T0 + LEASE_MS, || 0.0).unwrap();
        assert_eq!(recovery.recovered, 1);
        assert_eq!(recovery.unreadable.len(), 1);
        assert_eq!(l.task("t2").unwrap().status, Status::Retrying);
        let rows: Vec<IdRow> = db
            .query(
                "SELECT id FROM tasks WHERE status = 'failed' AND lease_until_ms IS NULL",
                vec![],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "t1");
        // Failed, it no longer wakes the ledger at its old lease time.
        assert_ne!(l.next_wake().unwrap(), Some(T0 + LEASE_MS));
        // Its attempt is closed, it says why, and its report reaches the inbox.
        let closed: Vec<IdRow> = db
            .query(
                "SELECT task_id AS id FROM attempts WHERE outcome = 'lease_expired' \
                 AND finished_ms IS NOT NULL ORDER BY task_id",
                vec![],
            )
            .unwrap();
        assert_eq!(
            closed.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["t1", "t2"]
        );
        let failed: Vec<IdRow> = db
            .query(
                "SELECT id FROM tasks WHERE failure = 'unreadable' AND inbox_seq IS NOT NULL",
                vec![],
            )
            .unwrap();
        assert_eq!(failed.len(), 1);
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

    /// One tick does the alarm's work in order: a cron run started in it is dispatched in it,
    /// and so is the alert for a run it found lost.
    #[test]
    fn a_tick_dispatches_what_it_starts() {
        let db = Memory::new();
        let l = setup(&db);
        assert!(l.tick(T0, || 0.0).unwrap().dispatches.is_empty());
        l.put_cron(daily("backup"), T0).unwrap();
        let tick = l.tick(SIX, || 0.0).unwrap();
        assert_eq!(tick.dispatches.len(), 1, "the run it started");
        assert!(tick.unreadable.is_empty());
        let d = &tick.dispatches[0];
        assert!(matches!(
            l.claim(&d.task_id, d.generation, SIX).unwrap(),
            Claimed::Deliver(_)
        ));

        // The consumer vanishes. The tick at the lease's end fails the run (its only attempt)
        // and dispatches the alert that failure raised.
        let after = l.tick(SIX + LEASE_MS, || 0.0).unwrap().dispatches;
        assert_eq!(after.len(), 1);
        assert_ne!(after[0].task_id, d.task_id);
        assert_eq!(alerts(&db, "backup").len(), 1);
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
}
