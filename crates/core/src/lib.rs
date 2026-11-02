//! Pure task domain: request validation, the task lifecycle, retry policy and limits.
//!
//! No Cloudflare dependencies, so everything here is tested natively with `cargo test`.
//! Time is always passed in as unix milliseconds; nothing here reads a clock or a RNG.
//! The API stores tasks as rows and drives them through [`Task`]'s transitions.

pub mod cron;
pub mod limits;
mod request;
mod retry;
mod task;
mod watch;

pub use cron::{valid_cron_name, CronError, CronRequest, CronRequestError, CronSpec, Schedule};
pub use request::{
    valid_lane, Callback, Method, ScheduleRequest, TaskSpec, ValidationError, INBOX,
};
pub use retry::{backoff_ms, classify, Outcome, Verdict};
pub use task::{Attempt, CancelError, Cancelled, ClaimError, RedriveError, Stale, Status, Task};
pub use watch::{
    judge, next_poll, Poll, PollBody, Until, Watch, WatchRequest, WatchRequestError, WatchSpec,
};
