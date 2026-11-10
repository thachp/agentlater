//! The task lifecycle as a state machine.
//!
//! ```text
//! scheduled ──claim──▶ delivering ──2xx──▶ succeeded
//!     │                   │
//!     │ cancel            ├─retryable, attempts left──▶ retrying ──claim──▶ delivering
//!     ▼                   │
//!  canceled ◀─cancel──────┤ (cancel while delivering takes effect when the attempt ends)
//!                         └─give up / attempts exhausted──▶ failed ──redrive──▶ scheduled
//! ```
//!
//! Two counters keep at-least-once transport safe:
//! - `generation` changes whenever the task is (re)scheduled, so a queued dispatch for an
//!   older schedule is ignored by [`Task::claim`].
//! - `fence` changes on every claim, so a late result from an earlier attempt is ignored
//!   by [`Task::finish`].

use crate::limits::LEASE_MS;
use crate::retry::{backoff_ms, classify, Outcome, Verdict};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Scheduled,
    Delivering,
    Retrying,
    Succeeded,
    Failed,
    Canceled,
}

impl Status {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Canceled)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub status: Status,
    /// When the next attempt may start. Meaningful while scheduled or retrying.
    pub due_ms: i64,
    pub max_attempts: u32,
    /// Attempts since the task was created or last redriven; bounded by `max_attempts`.
    pub run_attempts: u32,
    /// Attempts over the task's whole life; numbers each attempt.
    pub total_attempts: u32,
    pub generation: u64,
    pub fence: u64,
    pub lease_until_ms: Option<i64>,
    pub cancel_requested: bool,
    pub terminal_ms: Option<i64>,
}

/// A claimed attempt. The caller sends the request and reports back with `fence`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub number: u32,
    pub fence: u64,
    pub started_ms: i64,
    pub lease_until_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimError {
    /// The dispatch is for an old generation, or the task is not waiting to run.
    Stale,
    /// Right generation, but too early (e.g. the queue delivered ahead of a backoff).
    NotDue { due_ms: i64 },
}

/// A result for an attempt that no longer holds the task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stale;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cancelled {
    /// The task is now canceled (or already was).
    Done,
    /// An attempt is in flight; the task becomes canceled when it ends without success.
    Pending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelError {
    AlreadyFinished(Status),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedriveError {
    NotFailed(Status),
}

impl Task {
    pub fn new(run_at_ms: i64, max_attempts: u32) -> Self {
        Self {
            status: Status::Scheduled,
            due_ms: run_at_ms,
            max_attempts,
            run_attempts: 0,
            total_attempts: 0,
            generation: 1,
            fence: 0,
            lease_until_ms: None,
            cancel_requested: false,
            terminal_ms: None,
        }
    }

    pub fn is_due(&self, now_ms: i64) -> bool {
        matches!(self.status, Status::Scheduled | Status::Retrying) && self.due_ms <= now_ms
    }

    /// Starts an attempt for a dispatch carrying `generation`.
    pub fn claim(&mut self, generation: u64, now_ms: i64) -> Result<Attempt, ClaimError> {
        if generation != self.generation
            || !matches!(self.status, Status::Scheduled | Status::Retrying)
        {
            return Err(ClaimError::Stale);
        }
        if self.due_ms > now_ms {
            return Err(ClaimError::NotDue {
                due_ms: self.due_ms,
            });
        }
        self.status = Status::Delivering;
        self.fence += 1;
        self.run_attempts += 1;
        self.total_attempts += 1;
        let lease_until_ms = now_ms + LEASE_MS;
        self.lease_until_ms = Some(lease_until_ms);
        Ok(Attempt {
            number: self.total_attempts,
            fence: self.fence,
            started_ms: now_ms,
            lease_until_ms,
        })
    }

    /// Records the result of the attempt holding `fence`. `jitter` in `[0, 1)` spreads
    /// retries; the caller supplies it so this stays deterministic.
    ///
    /// A result that arrives after its lease expired is still accepted if recovery has
    /// not yet run: the real answer beats an "unknown".
    pub fn finish(
        &mut self,
        fence: u64,
        outcome: Outcome,
        now_ms: i64,
        jitter: f64,
    ) -> Result<Status, Stale> {
        if self.status != Status::Delivering || self.fence != fence {
            return Err(Stale);
        }
        self.settle(outcome, now_ms, jitter);
        Ok(self.status)
    }

    /// Records the result of a watch's poll holding `fence`. `next_due` is when to poll
    /// again, or `None` if the watch is over; `matched` ends it successfully. Polling again
    /// is a fresh schedule, not a retry, so `max_attempts` never runs out.
    pub fn finish_poll(
        &mut self,
        fence: u64,
        next_due: Option<i64>,
        matched: bool,
        now_ms: i64,
    ) -> Result<Status, Stale> {
        if self.status != Status::Delivering || self.fence != fence {
            return Err(Stale);
        }
        self.lease_until_ms = None;
        self.status = match next_due {
            _ if matched => Status::Succeeded,
            _ if self.cancel_requested => Status::Canceled,
            Some(_) => Status::Scheduled,
            None => Status::Failed,
        };
        match next_due {
            Some(due) if self.status == Status::Scheduled => {
                self.due_ms = due;
                self.run_attempts = 0;
                self.generation += 1;
            }
            _ => self.terminal_ms = Some(now_ms),
        }
        Ok(self.status)
    }

    /// Recovery: if the in-flight attempt's lease has passed, treat it as lost.
    /// Returns the new status if anything changed.
    pub fn expire_lease(&mut self, now_ms: i64, jitter: f64) -> Option<Status> {
        if self.status == Status::Delivering && self.lease_until_ms.is_some_and(|t| t <= now_ms) {
            self.settle(Outcome::LeaseExpired, now_ms, jitter);
            return Some(self.status);
        }
        None
    }

    pub fn cancel(&mut self, now_ms: i64) -> Result<Cancelled, CancelError> {
        match self.status {
            Status::Scheduled | Status::Retrying => {
                self.status = Status::Canceled;
                self.terminal_ms = Some(now_ms);
                self.generation += 1;
                Ok(Cancelled::Done)
            }
            Status::Delivering => {
                self.cancel_requested = true;
                Ok(Cancelled::Pending)
            }
            Status::Canceled => Ok(Cancelled::Done),
            s @ (Status::Succeeded | Status::Failed) => Err(CancelError::AlreadyFinished(s)),
        }
    }

    /// Gives a failed task a fresh set of attempts, starting now. History is kept.
    pub fn redrive(&mut self, now_ms: i64) -> Result<(), RedriveError> {
        if self.status != Status::Failed {
            return Err(RedriveError::NotFailed(self.status));
        }
        self.status = Status::Scheduled;
        self.due_ms = now_ms;
        self.run_attempts = 0;
        self.generation += 1;
        self.cancel_requested = false;
        self.terminal_ms = None;
        Ok(())
    }

    /// Fails a waiting task without attempting it, e.g. because the project has no attempts
    /// left. Returns false if the task was not waiting.
    pub fn abandon(&mut self, now_ms: i64) -> bool {
        if !matches!(self.status, Status::Scheduled | Status::Retrying) {
            return false;
        }
        self.status = Status::Failed;
        self.terminal_ms = Some(now_ms);
        self.generation += 1;
        true
    }

    fn settle(&mut self, outcome: Outcome, now_ms: i64, jitter: f64) {
        self.lease_until_ms = None;
        let verdict = classify(&outcome);
        self.status = match verdict {
            Verdict::Success => Status::Succeeded,
            _ if self.cancel_requested => Status::Canceled,
            Verdict::Retry if self.run_attempts < self.max_attempts => Status::Retrying,
            _ => Status::Failed,
        };
        if self.status == Status::Retrying {
            let retry_after = match outcome {
                Outcome::Http { retry_after_ms, .. } => retry_after_ms,
                _ => None,
            };
            self.due_ms = now_ms + backoff_ms(self.run_attempts, retry_after, jitter);
            self.generation += 1;
        } else {
            self.terminal_ms = Some(now_ms);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_767_225_600_000;

    fn http(status: u16) -> Outcome {
        Outcome::Http {
            status,
            retry_after_ms: None,
        }
    }

    /// Claims and finishes one attempt at `now`.
    fn run(task: &mut Task, outcome: Outcome, now: i64) -> Status {
        let a = task.claim(task.generation, now).expect("claimable");
        task.finish(a.fence, outcome, now + 100, 0.0)
            .expect("current")
    }

    #[test]
    fn new_task_waits_until_due() {
        let mut t = Task::new(T0 + 5_000, 5);
        assert!(!t.is_due(T0));
        assert_eq!(
            t.claim(1, T0),
            Err(ClaimError::NotDue { due_ms: T0 + 5_000 })
        );
        assert!(t.is_due(T0 + 5_000));
    }

    #[test]
    fn succeeds_on_first_try() {
        let mut t = Task::new(T0, 5);
        let a = t.claim(1, T0).unwrap();
        assert_eq!(a.number, 1);
        assert_eq!(a.lease_until_ms, T0 + LEASE_MS);
        assert_eq!(t.status, Status::Delivering);
        assert_eq!(
            t.finish(a.fence, http(204), T0 + 50, 0.0),
            Ok(Status::Succeeded)
        );
        assert_eq!(t.terminal_ms, Some(T0 + 50));
    }

    #[test]
    fn retries_with_backoff_then_fails() {
        let mut t = Task::new(T0, 3);
        assert_eq!(run(&mut t, http(503), T0), Status::Retrying);
        assert_eq!(t.generation, 2);
        assert_eq!(t.due_ms, T0 + 100 + 5_000);
        let due = t.due_ms;
        assert_eq!(run(&mut t, Outcome::Timeout, due), Status::Retrying);
        let due = t.due_ms;
        assert_eq!(run(&mut t, http(500), due), Status::Failed);
        assert_eq!(t.total_attempts, 3);
    }

    #[test]
    fn permanent_errors_fail_immediately() {
        let mut t = Task::new(T0, 5);
        assert_eq!(run(&mut t, http(404), T0), Status::Failed);
        assert_eq!(t.run_attempts, 1);
    }

    #[test]
    fn honours_retry_after() {
        let mut t = Task::new(T0, 5);
        let a = t.claim(1, T0).unwrap();
        let outcome = Outcome::Http {
            status: 429,
            retry_after_ms: Some(120_000),
        };
        t.finish(a.fence, outcome, T0, 0.0).unwrap();
        assert_eq!(t.due_ms, T0 + 120_000);
    }

    #[test]
    fn ignores_old_dispatches_and_double_claims() {
        let mut t = Task::new(T0, 5);
        run(&mut t, http(503), T0);
        assert_eq!(t.claim(1, t.due_ms), Err(ClaimError::Stale));
        t.claim(2, t.due_ms).unwrap();
        assert_eq!(t.claim(2, t.due_ms), Err(ClaimError::Stale));
    }

    #[test]
    fn ignores_late_results_from_earlier_attempts() {
        let mut t = Task::new(T0, 5);
        let first = t.claim(1, T0).unwrap();
        assert_eq!(
            t.expire_lease(first.lease_until_ms, 0.0),
            Some(Status::Retrying)
        );
        assert_eq!(
            t.finish(first.fence, http(200), T0 + 40_000, 0.0),
            Err(Stale)
        );
        let second = t.claim(t.generation, t.due_ms).unwrap();
        assert_eq!(second.number, 2);
        assert_eq!(t.finish(first.fence, http(200), t.due_ms, 0.0), Err(Stale));
        assert_eq!(
            t.finish(second.fence, http(200), t.due_ms, 0.0),
            Ok(Status::Succeeded)
        );
    }

    #[test]
    fn accepts_a_late_result_before_recovery_runs() {
        let mut t = Task::new(T0, 5);
        let a = t.claim(1, T0).unwrap();
        assert_eq!(
            t.finish(a.fence, http(200), a.lease_until_ms + 1, 0.0),
            Ok(Status::Succeeded)
        );
    }

    #[test]
    fn lease_only_expires_after_its_deadline() {
        let mut t = Task::new(T0, 1);
        let a = t.claim(1, T0).unwrap();
        assert_eq!(t.expire_lease(a.lease_until_ms - 1, 0.0), None);
        assert_eq!(t.expire_lease(a.lease_until_ms, 0.0), Some(Status::Failed));
        assert_eq!(t.expire_lease(a.lease_until_ms + 1, 0.0), None);
    }

    #[test]
    fn cancel_before_delivery() {
        let mut t = Task::new(T0 + 60_000, 5);
        assert_eq!(t.cancel(T0), Ok(Cancelled::Done));
        assert_eq!(t.status, Status::Canceled);
        assert_eq!(t.claim(1, T0 + 60_000), Err(ClaimError::Stale));
        assert_eq!(t.claim(2, T0 + 60_000), Err(ClaimError::Stale));
        assert_eq!(t.cancel(T0 + 1), Ok(Cancelled::Done));
    }

    #[test]
    fn cancel_during_delivery_waits_for_the_attempt() {
        let mut t = Task::new(T0, 5);
        let a = t.claim(1, T0).unwrap();
        assert_eq!(t.cancel(T0 + 1), Ok(Cancelled::Pending));
        assert_eq!(
            t.finish(a.fence, http(503), T0 + 2, 0.0),
            Ok(Status::Canceled)
        );

        let mut t = Task::new(T0, 5);
        let a = t.claim(1, T0).unwrap();
        t.cancel(T0 + 1).unwrap();
        // It already happened; report the truth.
        assert_eq!(
            t.finish(a.fence, http(200), T0 + 2, 0.0),
            Ok(Status::Succeeded)
        );
    }

    #[test]
    fn cannot_cancel_finished_tasks() {
        let mut t = Task::new(T0, 5);
        run(&mut t, http(200), T0);
        assert_eq!(
            t.cancel(T0 + 1),
            Err(CancelError::AlreadyFinished(Status::Succeeded))
        );
    }

    #[test]
    fn redrive_restarts_a_failed_task() {
        let mut t = Task::new(T0, 1);
        run(&mut t, http(500), T0);
        assert_eq!(t.status, Status::Failed);
        let old_generation = t.generation;

        t.redrive(T0 + 1_000).unwrap();
        assert_eq!(t.status, Status::Scheduled);
        assert_eq!(t.run_attempts, 0);
        assert_eq!(t.terminal_ms, None);
        assert_eq!(t.claim(old_generation, T0 + 1_000), Err(ClaimError::Stale));

        assert_eq!(run(&mut t, http(200), T0 + 1_000), Status::Succeeded);
        assert_eq!(t.total_attempts, 2);
    }

    #[test]
    fn only_failed_tasks_can_be_redriven() {
        let mut t = Task::new(T0, 5);
        assert_eq!(
            t.redrive(T0),
            Err(RedriveError::NotFailed(Status::Scheduled))
        );
        run(&mut t, http(200), T0);
        assert_eq!(
            t.redrive(T0),
            Err(RedriveError::NotFailed(Status::Succeeded))
        );
    }

    #[test]
    fn abandon_fails_only_waiting_tasks() {
        let mut t = Task::new(T0, 5);
        assert!(t.abandon(T0));
        assert_eq!(t.status, Status::Failed);
        assert_eq!(t.claim(1, T0), Err(ClaimError::Stale));
        assert!(!t.abandon(T0));

        let mut t = Task::new(T0, 5);
        t.claim(1, T0).unwrap();
        assert!(!t.abandon(T0));
        assert_eq!(t.status, Status::Delivering);
    }

    #[test]
    fn polls_reschedule_without_using_up_attempts() {
        let mut t = Task::new(T0, 1);
        for n in 1..=3 {
            let a = t.claim(t.generation, t.due_ms).unwrap();
            assert_eq!(a.number, n);
            let next = t.due_ms + 300_000;
            assert_eq!(
                t.finish_poll(a.fence, Some(next), false, t.due_ms),
                Ok(Status::Scheduled)
            );
            assert_eq!((t.due_ms, t.run_attempts, t.terminal_ms), (next, 0, None));
        }
        let stale = t.generation - 1;
        assert_eq!(t.claim(stale, t.due_ms), Err(ClaimError::Stale));
        let a = t.claim(t.generation, t.due_ms).unwrap();
        assert_eq!(
            t.finish_poll(a.fence, None, true, T0 + 1),
            Ok(Status::Succeeded)
        );
        assert_eq!(t.terminal_ms, Some(T0 + 1));
        assert_eq!(t.finish_poll(a.fence, None, true, T0 + 2), Err(Stale));
    }

    #[test]
    fn a_poll_ends_failed_or_canceled() {
        let mut t = Task::new(T0, 1);
        let a = t.claim(1, T0).unwrap();
        assert_eq!(t.finish_poll(a.fence, None, false, T0), Ok(Status::Failed));

        let mut t = Task::new(T0, 1);
        let a = t.claim(1, T0).unwrap();
        t.cancel(T0).unwrap();
        assert_eq!(
            t.finish_poll(a.fence, Some(T0 + 300_000), false, T0),
            Ok(Status::Canceled)
        );
        assert_eq!(t.terminal_ms, Some(T0));

        // A match while a cancel waits is still the truth.
        let mut t = Task::new(T0, 1);
        let a = t.claim(1, T0).unwrap();
        t.cancel(T0).unwrap();
        assert_eq!(
            t.finish_poll(a.fence, None, true, T0),
            Ok(Status::Succeeded)
        );
    }

    #[test]
    fn serializes_status_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&Status::Delivering).unwrap(),
            "\"delivering\""
        );
        let t = Task::new(T0, 5);
        let round: Task = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!(round, t);
    }
}
