//! Attempts: publishing due tasks to the queue, claiming one to start an attempt, and
//! recording how the attempt went, or that its consumer vanished.

use super::*;

/// A published task that has not been claimed after this long is published again.
pub(super) const REPUBLISH_MS: i64 = 60_000;
/// Most tasks published per alarm; the alarm re-arms immediately if more are due.
const PUBLISH_BATCH: usize = 100;

/// SQL condition, over `tasks t`: the task may run now as far as its lane is concerned.
/// A lane runs one task at a time. A task that has started (delivering, or retrying after a
/// failed attempt) holds the lane until it finishes; otherwise the waiting task with the
/// earliest due time, then the earliest scheduled, goes next. Finished tasks never block.
pub(super) const LANE_CLEAR: &str =
    "(t.lane IS NULL OR t.status IN ('delivering', 'retrying') OR NOT EXISTS ( \
    SELECT 1 FROM tasks o WHERE o.lane = t.lane AND o.seq != t.seq AND ( \
    o.status IN ('delivering', 'retrying') OR (o.status = 'scheduled' AND \
    (o.due_ms < t.due_ms OR (o.due_ms = t.due_ms AND o.seq < t.seq))))))";

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
    /// The secret a rotation replaced, while its overlap lasts: it signs too. Left out when
    /// there is none, so a ledger and a consumer on either side of a deploy read each other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_secret_previous: Option<String>,
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

impl Delivery {
    /// The secrets that sign this delivery, the current one first.
    pub fn signing_secrets(&self) -> Vec<&str> {
        std::iter::once(&self.signing_secret)
            .chain(&self.signing_secret_previous)
            .map(String::as_str)
            .collect()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Claimed {
    Deliver(Box<Delivery>),
    /// Nothing to do for this message; acknowledge it.
    Skip(&'static str),
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

impl<D: Db> Ledger<'_, D> {
    /// Due tasks to put on the queue. Marks them published so the next alarm does not
    /// republish them until [`REPUBLISH_MS`] has passed without a claim.
    pub(super) fn publish_due(&self, project_id: &str, now: i64) -> Result<Vec<Dispatch>, DbError> {
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
        let (signing_secret, signing_secret_previous) = project.signing_secrets(now);
        Ok(Claimed::Deliver(Box::new(Delivery {
            task_id: task_id.into(),
            attempt: attempt.number,
            fence: attempt.fence,
            method: parse(&row.method)?,
            url: row.url.clone(),
            headers: row.headers()?,
            body,
            signing_secret,
            signing_secret_previous,
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
    /// own, so it can't hold up the others (or wake the ledger again every time); returns
    /// why each such row could not be read.
    pub(super) fn recover(
        &self,
        now: i64,
        mut jitter: impl FnMut() -> f64,
    ) -> Result<Vec<DbError>, DbError> {
        let rows: Vec<TaskRow> = self.db.query(
            &format!(
                "SELECT {TASK_COLUMNS} FROM tasks \
                 WHERE status = 'delivering' AND lease_until_ms <= ?"
            ),
            vec![now.into()],
        )?;
        let mut unreadable = vec![];
        for row in &rows {
            let (mut task, kind) = match row.task().and_then(|t| Ok((t, row.kind()?))) {
                Ok(read) => read,
                Err(e) => {
                    unreadable.push(e);
                    if let Err(e) = self.fail_unreadable(row, now) {
                        unreadable.push(e);
                    }
                    continue;
                }
            };
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
        Ok(unreadable)
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

    fn lane_clear(&self, task_id: &str) -> Result<bool, DbError> {
        let rows: Vec<IdRow> = self.db.query(
            &format!("SELECT id FROM tasks t WHERE id = ? AND {LANE_CLEAR}"),
            vec![task_id.into()],
        )?;
        Ok(!rows.is_empty())
    }

    /// Attempt numbers and fences advance together, so the fence names the attempt row.
    pub(super) fn record_outcome(
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
pub(super) fn excerpt(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[derive(Deserialize)]
struct DueRow {
    id: String,
    generation: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::Memory;
    use crate::ledger::testing::*;
    use runlater_core::limits::LEASE_MS;

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
            (d.attempt, d.fence, d.signing_secrets()),
            (1, 1, vec!["secret"])
        );
        assert_eq!(l.task("t1").unwrap().status, Status::Delivering);
        assert_eq!(l.project(T0).unwrap().attempts_used, 1);

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
            l.project(T0).unwrap().attempts_used,
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

        assert!(l.recover(T0 + LEASE_MS - 1, || 0.0).unwrap().is_empty());
        assert_eq!(l.task("t1").unwrap().status, Status::Delivering);
        assert!(l.recover(T0 + LEASE_MS, || 0.0).unwrap().is_empty());
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

        let unreadable = l.recover(T0 + LEASE_MS, || 0.0).unwrap();
        assert_eq!(unreadable.len(), 1);
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

        let listed = l
            .list(None, None, None, None, None)
            .unwrap()
            .tasks
            .remove(0);
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
