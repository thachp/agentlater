//! The attempt budget: what work costs, what is already promised, and who may spend.
//!
//! A project buys attempts. Admission refuses work the budget can't cover, counting what
//! earlier work was promised: its first attempt, the callback it will owe, and a watch's
//! action. Retries are never promised; they spend what is left. When an attempt is claimed,
//! [`verdict`] decides whether it may spend: a callback or alert reports on work already
//! done, so it gets one try past the budget, as does a watch's action, once; a wake was never
//! admitted, so it may only spend what no waiting work was promised.

use super::*;
use std::ops::Add;

/// What admitting some work sets aside: rows in `tasks`, and attempts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Cost {
    pub rows: i64,
    pub attempts: i64,
}

impl Cost {
    /// A new task: its row and first attempt, and the same again for the callback task it will
    /// queue when it finishes, if it reports to a URL. An inbox report is not a task.
    pub(super) fn task(reports_to: Option<&str>) -> Cost {
        Cost::follow_up(reports_to)
            + Cost {
                rows: 1,
                attempts: 1,
            }
    }

    /// A failed task sent again: another first attempt, and another callback. Its row exists.
    pub(super) fn redrive(reports_to: Option<&str>) -> Cost {
        Cost::follow_up(reports_to)
            + Cost {
                rows: 0,
                attempts: 1,
            }
    }

    fn follow_up(reports_to: Option<&str>) -> Cost {
        let n = i64::from(reports_to.is_some_and(|to| to != INBOX));
        Cost {
            rows: n,
            attempts: n,
        }
    }

    /// What a watch's action reserves, stored on the watch until it ends. Rows and attempts
    /// are the same here, so one number holds it.
    pub(super) fn reserve(self) -> i64 {
        debug_assert_eq!(self.rows, self.attempts);
        self.attempts
    }
}

impl Add for Cost {
    type Output = Cost;
    fn add(self, other: Cost) -> Cost {
        Cost {
            rows: self.rows + other.rows,
            attempts: self.attempts + other.attempts,
        }
    }
}

/// SQL, over `tasks`: the follow-ups a task still owes, in rows and in attempts: the callback
/// task it will queue, and a watch's action and its callback until the watch ends.
const OWES: &str = "CASE WHEN status IN ('scheduled', 'retrying', 'delivering') \
    THEN (callback_url IS NOT NULL AND callback_url != 'inbox') + action_reserve ELSE 0 END";

/// What admission promised and has not yet been spent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
struct Owed {
    /// Rows held, plus rows owed follow-ups will need.
    #[serde(rename = "owed_rows")]
    rows: i64,
    /// Attempts promised to work waiting for them.
    attempts: i64,
}

/// How the project stands when an attempt is claimed.
#[derive(Debug, Clone, Copy)]
pub(super) struct Standing {
    /// Within the trial or a credit's validity, with attempts left.
    pub can_attempt: bool,
    /// Attempts left; none once the project has expired.
    pub left: i64,
    pub inbox_unread: i64,
}

/// Whether a claimed attempt may spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Spend from the budget as usual.
    Spend,
    /// The budget is gone, but this one gets a final try, and no retries.
    LastTry,
    /// The budget is gone: the task is abandoned.
    Exhausted,
    /// A wake that should not be sent: canceled, not failed, for this reason.
    StandDown(&'static str),
}

/// Decides whether an attempt at a task of `kind` may spend. `current` says the dispatch is
/// for the task's present schedule and it is due; `reserved` reads the attempts promised to
/// waiting work, and is called only for a wake.
pub(super) fn verdict(
    kind: Kind<'_>,
    task: &Task,
    current: bool,
    standing: Standing,
    reserved: impl FnOnce() -> Result<i64, DbError>,
) -> Result<Verdict, DbError> {
    let mut last_try = match kind {
        Kind::Callback { .. } | Kind::Alert { .. } => task.run_attempts == 0,
        // Admission reserved its first attempt; a redrive of it is admitted like any task's.
        Kind::Action { .. } => task.total_attempts == 0,
        _ => false,
    };
    // With nothing else waiting a wake gets one more try even past the budget; otherwise it
    // stands down. A wake due after the agent read everything has nothing to say.
    if kind.is_wake() && current {
        if standing.inbox_unread == 0 {
            return Ok(Verdict::StandDown("inbox_read"));
        }
        let reserved = reserved()?;
        if standing.left - reserved >= 1 {
        } else if reserved == 0 {
            last_try = true;
        } else {
            return Ok(Verdict::StandDown("budget_reserved"));
        }
    }
    Ok(match (standing.can_attempt, last_try) {
        (true, _) => Verdict::Spend,
        (false, true) => Verdict::LastTry,
        (false, false) => Verdict::Exhausted,
    })
}

impl<D: Db> Ledger<'_, D> {
    /// Refuses work the project can't take: it has expired, holds too many tasks, or hasn't
    /// the attempts for `cost` beyond those already promised.
    pub(super) fn admit(&self, project: &ProjectRow, cost: Cost, now: i64) -> Result<(), ApiError> {
        if now >= project.expires_ms {
            return Err(ApiError::new(
                403,
                "trial_expired",
                "this project's attempts have expired",
            ));
        }
        let owed = self.owed_to_admitted()?;
        let cap = project.max_retained_tasks();
        if cost.rows > 0 && owed.rows + cost.rows > cap {
            return Err(ApiError::new(
                429,
                "task_limit",
                format!(
                    "this project already holds {cap} tasks; \
                     finished tasks are removed 30 days after they finish"
                ),
            ));
        }
        if owed.attempts + cost.attempts > project.attempt_budget - project.attempts_used {
            return Err(ApiError::new(
                402,
                "budget_exhausted",
                "not enough delivery attempts left in this project",
            ));
        }
        Ok(())
    }

    /// Rows held and owed, and attempts promised, as admission counts them.
    fn owed_to_admitted(&self) -> Result<Owed, DbError> {
        let owed: Vec<Owed> = self.db.query(
            &format!(
                "SELECT COUNT(*) + COALESCE(SUM({OWES}), 0) AS owed_rows, \
                 COALESCE(SUM(CASE WHEN status IN ('scheduled', 'retrying', 'delivering') \
                 THEN 1 ELSE 0 END + {OWES}), 0) AS attempts \
                 FROM tasks"
            ),
            vec![],
        )?;
        Ok(owed.first().copied().unwrap_or_default())
    }

    /// Attempts promised to waiting work, as a wake counts them: one per task not yet
    /// attempted (scheduled or retrying) and its owed follow-ups. Wakes are not counted.
    pub(super) fn reserved_attempts(&self) -> Result<i64, DbError> {
        let owed: Vec<Owed> = self.db.query(
            &format!(
                "SELECT 0 AS owed_rows, COALESCE(SUM(status IN ('scheduled', 'retrying')), 0) \
                 + COALESCE(SUM({OWES}), 0) AS attempts FROM tasks WHERE wake_call = 0"
            ),
            vec![],
        )?;
        Ok(owed.first().map_or(0, |o| o.attempts))
    }

    /// Charges one attempt.
    pub(super) fn charge(&self) -> Result<(), DbError> {
        self.db.exec(
            "UPDATE project SET attempts_used = attempts_used + 1",
            vec![],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn standing(can_attempt: bool, left: i64, inbox_unread: i64) -> Standing {
        Standing {
            can_attempt,
            left,
            inbox_unread,
        }
    }

    fn tried(run_attempts: u32, total_attempts: u32) -> Task {
        Task {
            run_attempts,
            total_attempts,
            ..Task::new(0, 3)
        }
    }

    fn no_reads() -> Result<i64, DbError> {
        panic!("only a wake reads what is reserved")
    }

    #[test]
    fn costs_count_the_task_and_a_url_callback_but_not_an_inbox_report() {
        let one = Cost {
            rows: 1,
            attempts: 1,
        };
        assert_eq!(Cost::task(None), one);
        assert_eq!(Cost::task(Some(INBOX)), one);
        assert_eq!(Cost::task(Some("https://a.example/")), one + one);
        assert_eq!(
            Cost::redrive(Some("https://a.example/")),
            Cost {
                rows: 1,
                attempts: 2
            }
        );
        assert_eq!(
            Cost::redrive(Some(INBOX)),
            Cost {
                rows: 0,
                attempts: 1
            }
        );
        assert_eq!(Cost::task(Some("https://a.example/")).reserve(), 2);
    }

    #[test]
    fn with_budget_left_everything_spends() {
        let ok = standing(true, 10, 1);
        for kind in [
            Kind::Request { lane: None },
            Kind::Callback { task: "t" },
            Kind::Action { watch: "w" },
        ] {
            assert_eq!(
                verdict(kind, &tried(0, 0), true, ok, no_reads).unwrap(),
                Verdict::Spend
            );
        }
    }

    #[test]
    fn past_the_budget_only_follow_ups_get_a_last_try() {
        let gone = standing(false, 0, 0);
        let v = |kind, task: Task| verdict(kind, &task, true, gone, no_reads).unwrap();
        assert_eq!(
            v(Kind::Request { lane: None }, tried(0, 0)),
            Verdict::Exhausted
        );
        assert_eq!(
            v(Kind::CronRun { cron: "c" }, tried(0, 0)),
            Verdict::Exhausted
        );
        assert_eq!(
            v(Kind::Callback { task: "t" }, tried(0, 0)),
            Verdict::LastTry
        );
        assert_eq!(v(Kind::Alert { cron: "c" }, tried(0, 0)), Verdict::LastTry);
        assert_eq!(
            v(Kind::Callback { task: "t" }, tried(1, 1)),
            Verdict::Exhausted,
            "one try, no retries"
        );
        assert_eq!(
            v(Kind::Action { watch: "w" }, tried(0, 0)),
            Verdict::LastTry
        );
        assert_eq!(
            v(Kind::Action { watch: "w" }, tried(0, 2)),
            Verdict::Exhausted,
            "a redriven action is admitted like any task"
        );
    }

    #[test]
    fn a_wake_spends_only_what_no_waiting_work_was_promised() {
        let wake = |s: Standing, reserved: i64| {
            verdict(Kind::Wake, &tried(0, 0), true, s, || Ok(reserved)).unwrap()
        };
        assert_eq!(
            wake(standing(true, 5, 0), 0),
            Verdict::StandDown("inbox_read")
        );
        assert_eq!(wake(standing(true, 5, 2), 4), Verdict::Spend);
        assert_eq!(
            wake(standing(true, 5, 2), 5),
            Verdict::StandDown("budget_reserved")
        );
        assert_eq!(
            wake(standing(false, 0, 2), 0),
            Verdict::LastTry,
            "nothing else waiting: one more try"
        );
        assert_eq!(
            verdict(
                Kind::Wake,
                &tried(0, 0),
                false,
                standing(false, 0, 2),
                no_reads
            )
            .unwrap(),
            Verdict::Exhausted,
            "a stale wake dispatch is not judged as a wake"
        );
    }
}
