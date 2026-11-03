//! Product limits from `docs/spec/PRODUCT.md`. Change them there first.

pub const SECOND_MS: i64 = 1_000;
pub const MINUTE_MS: i64 = 60 * SECOND_MS;
pub const HOUR_MS: i64 = 60 * MINUTE_MS;
pub const DAY_MS: i64 = 86_400_000;

pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const MAX_HEADERS: usize = 32;
/// Sum of name and value lengths across caller-supplied headers.
pub const MAX_HEADER_BYTES: usize = 8 * 1024;
pub const MAX_SCHEDULE_AHEAD_MS: i64 = 7 * DAY_MS;
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;
pub const MAX_LANE_BYTES: usize = 64;

/// Crons a project may hold.
pub const MAX_CRONS: i64 = 10;
/// Shortest gap allowed between two runs of one cron. The trial has 100 attempts over 7
/// days, so an hourly cron already uses it up in about four days.
pub const MIN_CRON_INTERVAL_MS: i64 = HOUR_MS;

/// Shortest and longest gap between two polls of a watch. Every poll costs an attempt.
pub const MIN_WATCH_INTERVAL_MS: i64 = 5 * MINUTE_MS;
pub const MAX_WATCH_INTERVAL_MS: i64 = DAY_MS;
/// Polls in a row that may fail (5xx, timeout, network) before a watch gives up.
pub const MAX_POLL_ERRORS: u32 = 3;
/// How much of a polled response is read to match against.
pub const MAX_WATCH_RESPONSE_BYTES: usize = 64 * 1024;
/// How much of the last polled response is kept and reported.
pub const WATCH_EXCERPT_BYTES: usize = 16 * 1024;

/// How much of a task's response `capture` keeps, and which response headers.
pub const CAPTURE_BODY_BYTES: usize = 16 * 1024;
pub const CAPTURED_HEADERS: [&str; 4] = ["content-type", "location", "etag", "retry-after"];
/// A captured response is deleted this long after its task finishes. Responses can hold
/// credentials, so they go well before the task does.
pub const CAPTURE_RETENTION_MS: i64 = 7 * DAY_MS;

/// Events a project's inbox holds. When full, the oldest acked event goes first.
pub const MAX_INBOX_EVENTS: i64 = 10_000;
/// Unread events are kept as long as finished tasks; acked ones this long after they arrive,
/// so recent history can be replayed.
pub const INBOX_ACKED_RETENTION_MS: i64 = 7 * DAY_MS;
pub const INBOX_DEFAULT_PAGE: usize = 10;
/// Kept small: every event read lands in the agent's context, and one with a captured
/// response can be ~20 KiB.
pub const INBOX_MAX_PAGE: usize = 50;

/// Debounce for `set_wake`: at most one wake per window, default 15 minutes.
pub const DEFAULT_WAKE_DEBOUNCE: &str = "15m";
pub const MIN_WAKE_DEBOUNCE_MS: i64 = MINUTE_MS;
pub const MAX_WAKE_DEBOUNCE_MS: i64 = DAY_MS;
/// Wakes in a row that may fail (all their attempts) before the wake turns itself off.
pub const MAX_WAKE_FAILURES: i64 = 3;

/// Secrets a project may hold, and how big each may be.
pub const MAX_SECRETS: i64 = 50;
pub const MAX_SECRET_BYTES: usize = 4 * 1024;
/// Shorter values are refused: redaction replaces every occurrence of a value, so a short one
/// could be found by reflecting guesses off an allowed host and seeing which come back redacted.
pub const MIN_SECRET_BYTES: usize = 16;
pub const MAX_SECRET_NAME_BYTES: usize = 64;
/// Host patterns one secret may allow.
pub const MAX_ALLOWED_HOSTS: usize = 20;

pub const DEFAULT_MAX_ATTEMPTS: u32 = 5;
pub const MAX_ATTEMPTS: u32 = 5;
pub const ATTEMPT_TIMEOUT_MS: i64 = 10 * SECOND_MS;
/// How long a claimed attempt may run before recovery treats its result as unknown.
/// Must comfortably exceed the attempt timeout.
pub const LEASE_MS: i64 = 30 * SECOND_MS;

pub const RETRY_BASE_MS: i64 = 10 * SECOND_MS;
pub const RETRY_CAP_MS: i64 = 10 * MINUTE_MS;

/// Tasks a project may hold at once, finished ones included until they are purged.
/// Bounds storage even when tasks are scheduled and canceled without using attempts.
pub const MAX_RETAINED_TASKS: i64 = 1_000;
/// The same cap once a project has bought attempts: a pack covers far more tasks than a trial.
pub const MAX_RETAINED_TASKS_PAID: i64 = 20_000;

/// Terminal tasks and their attempts are kept this long, then purged.
pub const RETENTION_MS: i64 = 30 * DAY_MS;

pub const TRIAL_ATTEMPTS: u32 = 100;
pub const TRIAL_DURATION_MS: i64 = 7 * DAY_MS;

/// Bought attempts, and any still unused, stay valid this long after each purchase.
pub const CREDIT_VALIDITY_MS: i64 = 365 * DAY_MS;
/// Most attempts one payment may add; a sanity bound, well above any pack.
pub const MAX_CREDIT_ATTEMPTS: i64 = 10_000_000;

const _: () = assert!(LEASE_MS > 2 * ATTEMPT_TIMEOUT_MS);
const _: () = assert!(DEFAULT_MAX_ATTEMPTS <= MAX_ATTEMPTS);
