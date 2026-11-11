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
    DEFAULT_MAX_ATTEMPTS, DEFAULT_ROTATION_OVERLAP, MAX_CREDIT_ATTEMPTS, MAX_CRONS,
    MAX_HEADER_BYTES, MAX_RETAINED_TASKS, MAX_RETAINED_TASKS_PAID, MAX_ROTATION_OVERLAP_MS,
    MAX_SECRETS, RETENTION_MS, TRIAL_ATTEMPTS, TRIAL_DURATION_MS, WATCH_EXCERPT_BYTES,
};
use runlater_core::{
    fill, header_references, host_allowed, judge, next_poll, parse_delay, valid_lane, Attempt,
    CancelError, ClaimError, CronRequest, FieldError, Method, Outcome, Poll, PollBody,
    RedriveError, Report, Schedule, ScheduleRequest, Status, Task, Until, WakeRequest, Watch,
    WatchRequest, INBOX,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

mod attempts;
mod budget;
mod crons;
mod error;
mod hooks;
mod inbox;
mod project;
mod schema;
mod secrets;
mod tasks;
mod watches;
use attempts::{excerpt, LANE_CLEAR, REPUBLISH_MS};
pub use attempts::{Claimed, Delivery, Dispatch, ResponseRead};
use budget::{verdict, Cost, Standing, Verdict};
pub use crons::{CronList, CronView};
pub use error::ApiError;
pub use hooks::{
    token_hash, CreatedHook, HookDelivery, HookList, HookOutcome, HookReply, HookRow, HookView,
    Received,
};
pub use inbox::{InboxCursor, InboxPage, WakeView};
use project::{no_project, ProjectRow};
pub use project::{ProjectView, Rotated};
pub use runlater_core::Health;
use schema::{MIGRATIONS, SCHEMA};
pub use secrets::{ResealCount, SealedSecret, Sealer, SecretList, SecretView};
use tasks::{derived_id, Kind, NewTask, Reporting, TaskRow, TASK_COLUMNS};
pub use tasks::{AttemptView, CapturedResponse, ListQuery, TaskPage, TaskView};
pub use watches::{OnMatchView, WatchView};

/// Durable Object SQL bindings pass through JavaScript numbers.
const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

/// What a [`Ledger::tick`] leaves its caller: tasks to put on the queue, already marked
/// published, and the rows it could not read (failed where they stood).
#[derive(Debug, Default)]
pub struct Tick {
    pub dispatches: Vec<Dispatch>,
    pub unreadable: Vec<DbError>,
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

pub struct Ledger<'a, D: Db> {
    db: &'a D,
    /// The keys secrets are sealed with. Without them nothing can be sealed, and work that
    /// needs a secret fails before it is attempted rather than spending an attempt it can't make.
    sealer: Option<Sealer>,
    /// Whether the request being answered came through A2A. Such a request marks the task it
    /// schedules, and finds only tasks that carry the mark.
    a2a: bool,
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
            sealer: None,
            a2a: false,
        })
    }

    /// The ledger as a request that came through A2A sees it.
    pub fn through_a2a(self) -> Self {
        Self { a2a: true, ..self }
    }

    /// The ledger for a deployment that has keys to seal secrets with.
    pub fn with_sealer(self, sealer: Option<Sealer>) -> Self {
        Self { sealer, ..self }
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
        let unreadable = self.recover(now, jitter)?;
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

    /// The number a `SELECT ... AS total` comes to.
    fn count(&self, sql: &str, params: Vec<Param>) -> Result<i64, DbError> {
        let rows: Vec<Count> = self.db.query(sql, params)?;
        Ok(rows.first().map_or(0, |c| c.total))
    }

    /// Refuses a new cron, hook or secret (`table`) once the project has `max` of them.
    fn room_for(&self, table: &str, code: &'static str, max: i64) -> Result<(), ApiError> {
        if self.count(&format!("SELECT COUNT(*) AS total FROM {table}"), vec![])? < max {
            return Ok(());
        }
        let message = format!("this project already has {max} {table}; delete one first");
        Err(ApiError::new(429, code, message))
    }
}

#[derive(Deserialize)]
struct MetaRow {
    value: i64,
}

#[derive(Deserialize)]
struct Count {
    total: i64,
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
    fn opening_twice_is_harmless() {
        let db = Memory::new();
        setup(&db);
        assert!(Ledger::open(&db).is_ok());
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
    fn a_ledger_from_before_rotation_keeps_its_one_signing_secret() {
        let db = Memory::new();
        let before = MIGRATIONS[..12].iter().copied().flatten();
        for statement in SCHEMA.iter().chain(before) {
            db.0.execute(statement, []).unwrap();
        }
        db.0.execute("INSERT INTO meta (key, value) VALUES ('schema', 12)", [])
            .unwrap();
        db.0.execute(
            "INSERT INTO project (id, created_ms, expires_ms, attempt_budget, attempts_used, \
             signing_secret) VALUES (?1, ?2, ?3, 1000, 0, 'old')",
            (P, T0, T0 + DAY),
        )
        .unwrap();

        let l = Ledger::open(&db).unwrap();
        assert_eq!(
            l.project(T0).unwrap().signing_secret_previous_until_ms,
            None
        );
        l.schedule(req("https://a.example.com/"), "t1", T0).unwrap();
        let delivery = drive(&l, "t1", http(200), None, T0).0;
        assert_eq!(delivery.signing_secrets(), ["old"]);
        let rotated = l.rotate_signing_secret("new", None, T0).unwrap();
        assert_eq!(rotated.previous_valid_until_ms, Some(T0 + DAY));
    }

    #[test]
    fn a_ledger_from_before_a2a_keeps_its_tasks_and_none_is_read_through_a2a() {
        let db = Memory::new();
        let before = MIGRATIONS[..13].iter().copied().flatten();
        for statement in SCHEMA.iter().chain(before) {
            db.0.execute(statement, []).unwrap();
        }
        db.0.execute("INSERT INTO meta (key, value) VALUES ('schema', 13)", [])
            .unwrap();
        db.0.execute(
            "INSERT INTO project (id, created_ms, expires_ms, attempt_budget, attempts_used, \
             signing_secret) VALUES (?1, ?2, ?3, 1000, 0, 'old')",
            (P, T0, T0 + DAY),
        )
        .unwrap();
        db.0.execute(
            "INSERT INTO tasks (id, status, due_ms, max_attempts, run_attempts, total_attempts, \
             generation, fence, cancel_requested, method, url, headers, run_at_ms, fingerprint, \
             created_ms, updated_ms) VALUES ('old', 'scheduled', ?1, 3, 0, 0, 0, 0, 0, 'POST', \
             'https://a.example.com/', '[]', ?1, 'f', ?1, ?1)",
            [T0],
        )
        .unwrap();

        let l = Ledger::open(&db).unwrap();
        assert_eq!(l.task("old").unwrap().status, Status::Scheduled);
        assert_eq!(
            drive(&l, "old", http(200), None, T0).1,
            Some(Status::Succeeded)
        );
        let a2a = Ledger::open(&db).unwrap().through_a2a();
        assert_eq!(a2a.task("old").unwrap_err().code, "task_not_found");
        a2a.schedule(req("https://a.example.com/"), "new", T0)
            .unwrap();
        assert_eq!(a2a.task("new").unwrap().id, "new");
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

    #[test]
    fn through_a2a_a_purged_task_reads_like_one_that_never_was_or_was_made_another_way() {
        let db = Memory::new();
        let l = setup(&db);
        let a2a = Ledger::open(&db).unwrap().through_a2a();
        a2a.schedule(req("https://a.example.com/"), "t1", T0)
            .unwrap();
        drive(&a2a, "t1", http(200), None, T0);
        l.schedule(req("https://a.example.com/"), "t2", T0).unwrap();
        assert_eq!(a2a.task("t1").unwrap().status, Status::Succeeded);

        assert_eq!(l.purge(T0 + RETENTION_MS).unwrap(), 1);
        let purged = a2a.task("t1").unwrap_err();
        assert_eq!((purged.status, purged.code), (404, "task_not_found"));
        assert_eq!(a2a.task("t2").unwrap_err(), purged);
        assert_eq!(a2a.task("t3").unwrap_err(), purged);
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
}
