//! The inbox and the wake: where events for the agent wait, and the POST that says they're
//! there. Other parts of the ledger [post](Ledger::post) an event; numbering it, making room
//! in a full inbox, and queuing a debounced wake happen here, in the same transaction.

use super::*;
use runlater_core::limits::{
    INBOX_ACKED_RETENTION_MS, INBOX_DEFAULT_PAGE, INBOX_MAX_PAGE, MAX_INBOX_EVENTS,
    MAX_WAKE_FAILURES,
};

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

impl<D: Db> Ledger<'_, D> {
    /// Adds an event and wakes the agent for it. `event` is given the event's number, so a
    /// task's report can name the event that carries it.
    pub(super) fn post(
        &self,
        now: i64,
        event: impl FnOnce(i64) -> Result<serde_json::Value, DbError>,
    ) -> Result<i64, DbError> {
        let seq = self.next_inbox_seq()?;
        self.add_to_inbox(seq, &event(seq)?, now)?;
        self.wake_soon(now)?;
        Ok(seq)
    }

    /// Whether a hook event can be added. A full inbox makes room by dropping an acked event or
    /// an older hook event, never an unread result of the project's own work, so with none of
    /// those it is full for hooks.
    pub(super) fn room_for_hook_event(&self) -> Result<bool, DbError> {
        let inbox: Vec<HookRoom> = self.db.query(
            "SELECT COUNT(*) AS total, COALESCE(SUM(seq <= (SELECT inbox_acked_seq FROM project) \
             OR json_extract(event, '$.event') = 'hook.received'), 0) AS droppable FROM inbox",
            vec![],
        )?;
        Ok(inbox
            .first()
            .is_none_or(|c| c.total < MAX_INBOX_EVENTS || c.droppable > 0))
    }

    /// The project's wake setting, if any.
    pub(super) fn wake_view(&self) -> Result<Option<WakeView>, DbError> {
        self.wake_row()?.map(|w| w.view()).transpose()
    }

    /// A wake is being sent: it covers every event so far, the debounce counts from now, and
    /// its body is the unread count at this moment.
    pub(super) fn sending_wake(&self, project: &ProjectRow, now: i64) -> Result<String, DbError> {
        self.db.exec(
            "UPDATE wake SET covered_seq = ?, last_wake_ms = ?",
            vec![project.inbox_last_seq.into(), now.into()],
        )?;
        Ok(wake_payload(&project.id, project.inbox_unread))
    }

    /// The earliest time an inbox event or its captured body is due to go.
    pub(super) fn inbox_next_due(&self) -> Result<Option<i64>, DbError> {
        let rows: Vec<Due> = self.db.query(
            &format!(
                "SELECT MIN(t) AS t FROM ( \
                 SELECT MIN(at_ms) + {INBOX_ACKED_RETENTION_MS} AS t FROM inbox \
                   WHERE seq <= (SELECT inbox_acked_seq FROM project) \
                 UNION ALL SELECT MIN(at_ms) + {RETENTION_MS} FROM inbox \
                   WHERE seq > (SELECT inbox_acked_seq FROM project) \
                 UNION ALL SELECT MIN(response_until_ms) FROM inbox)"
            ),
            vec![],
        )?;
        Ok(rows.first().and_then(|w| w.t))
    }

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
        let spec = request.validate(now).map_err(ApiError::invalid)?;
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
        let headers = wake.headers()?;
        self.insert(
            NewTask::internal(
                &id,
                Kind::Wake,
                Task::new(due, DEFAULT_MAX_ATTEMPTS),
                Method::Post,
                &wake.url,
                &headers,
                // Rewritten when it is sent, with the count at that moment.
                Some(wake_payload(&project.id, project.inbox_unread)),
            ),
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
    pub(super) fn wake_finished(
        &self,
        task_id: &str,
        status: Status,
        now: i64,
    ) -> Result<(), DbError> {
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
                    // Not through post: this is the one event that must not wake (the wake is
                    // off, and this is why).
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
            return Err(ApiError::bad_request("after must not be negative").in_field("after"));
        }
        // Acking up to the last event returned would also ack the unread ones skipped over.
        if ack && after.is_some_and(|a| a > project.inbox_acked_seq) {
            return Err(ApiError::bad_request(format!(
                "with ack, after must not be past the acked cursor ({}); \
                 read without ack, or call ack_inbox",
                project.inbox_acked_seq
            ))
            .in_field("after"));
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
            return Err(ApiError::bad_request("up_to must not be negative").in_field("up_to"));
        }
        if up_to > project.inbox_last_seq {
            return Err(ApiError::new(
                400,
                "ack_beyond_inbox",
                format!(
                    "up_to is past the newest event ({}); ack only events you have read",
                    project.inbox_last_seq
                ),
            )
            .in_field("up_to"));
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
        let unread = self.count(
            "SELECT COUNT(*) AS total FROM inbox WHERE seq > ?",
            vec![acked_seq.into()],
        )?;
        Ok(InboxCursor { acked_seq, unread })
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
    /// event is unread, its oldest hook event, else its oldest one, counted in `dropped`.
    fn add_to_inbox(&self, seq: i64, event: &serde_json::Value, now: i64) -> Result<(), DbError> {
        // A captured body inside the event goes when the task's own copy does.
        let response_until = (event["task"]["response"]["body"].is_string()
            || event["request"]["body"].is_string())
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
        if self.count("SELECT COUNT(*) AS total FROM inbox", vec![])? <= MAX_INBOX_EVENTS {
            return Ok(());
        }
        // Unread hook events go before other unread events: they come from outside, and must
        // not push out the results of the project's own work.
        let oldest: Vec<OldestRow> = self.db.query(
            "SELECT seq, seq > (SELECT inbox_acked_seq FROM project) AS unread FROM inbox \
             ORDER BY unread, json_extract(event, '$.event') IS NOT 'hook.received', seq LIMIT 1",
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
    pub(super) fn purge_inbox(&self, now: i64) -> Result<(), DbError> {
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
        let stale = self.count(
            "SELECT COUNT(*) AS total FROM inbox WHERE seq > ? AND at_ms <= ?",
            vec![project.inbox_acked_seq.into(), (now - RETENTION_MS).into()],
        )?;
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

    /// Removes captured response bodies (and hook request bodies) from inbox events once the
    /// task's copy is gone.
    pub(super) fn expire_inbox_responses(&self, now: i64) -> Result<(), DbError> {
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
            // A hook event's body is someone else's request: it goes on the same clock.
            if let Some(request) = event["request"].as_object_mut() {
                request.insert("body".into(), serde_json::Value::Null);
            }
            self.db.exec(
                "UPDATE inbox SET event = ?, response_until_ms = NULL WHERE seq = ?",
                vec![event.to_string().into(), row.seq.into()],
            )?;
        }
        Ok(())
    }
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

/// The events the inbox holds, and how many of them a hook event may push out.
#[derive(Deserialize)]
struct HookRoom {
    total: i64,
    droppable: i64,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::Memory;
    use crate::ledger::testing::*;

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
        assert_eq!(l.project(T0).unwrap().inbox_unread, 1);
        assert_eq!(l.project(T0).unwrap().attempts_used, 1);

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
        assert_eq!(l.project(T0).unwrap().inbox_unread, 0);
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
            l.post(T0, |_| Ok(serde_json::json!({ "event": "test", "i": i })))
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
                l.post(T0, |_| Ok(serde_json::json!({ "event": "test" })))
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
            l.post(at, |_| Ok(serde_json::json!({ "event": "test" })))
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
            l.post(T0, |_| Ok(serde_json::json!({ "event": "test" })))
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
        l.project(T0)
            .unwrap()
            .wake
            .and_then(|w| w.last_wake_task_id)
    }

    /// When the pending wake is due.
    fn wake_due(l: &Ledger<'_, Memory>) -> Option<i64> {
        l.task(&wake_id(l)?).unwrap().next_attempt_ms
    }

    /// Sends the pending wake at `now` and answers it with `status`.
    fn send_wake(l: &Ledger<'_, Memory>, status: u16, now: i64) -> Delivery {
        let id = wake_id(l).expect("a wake was queued");
        drive(l, &id, http(status), None, now).0
    }

    #[test]
    fn a_pending_wake_holds_no_attempts_back_from_new_work() {
        let db = Memory::new();
        let l = setup(&db);
        db.0.execute("UPDATE project SET attempt_budget = 2", [])
            .unwrap();
        l.set_wake(waking(serde_json::json!({})), T0).unwrap();
        finish_to_inbox(&l, "t1", T0);
        assert_eq!(wake_tasks(&db), 1);
        // A wake only spends what nothing else was promised, so the last attempt is free.
        l.schedule(req("https://a.example.com/"), "t2", T0).unwrap();
    }

    #[test]
    fn no_wake_without_set_wake() {
        let db = Memory::new();
        let l = setup(&db);
        finish_to_inbox(&l, "t1", T0);
        assert_eq!(wake_tasks(&db), 0);
        assert_eq!(l.project(T0).unwrap().wake, None);
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
        assert_eq!(l.project(T0).unwrap().wake.unwrap().consecutive_failures, 0);
    }

    #[test]
    fn a_wake_is_signed_like_any_delivery_while_a_secret_is_rotated() {
        let db = Memory::new();
        let l = setup(&db);
        l.set_wake(waking(serde_json::json!({})), T0).unwrap();
        l.rotate_signing_secret("s2", Some("1h"), T0).unwrap();
        finish_to_inbox(&l, "t1", T0 + 5);
        assert_eq!(
            send_wake(&l, 202, T0 + 5).signing_secrets(),
            ["s2", "secret"]
        );
        finish_to_inbox(&l, "t2", T0 + DAY);
        assert_eq!(send_wake(&l, 202, T0 + DAY).signing_secrets(), ["s2"]);
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
        let used = l.project(T0).unwrap().attempts_used;
        assert_eq!(
            l.claim(&d.task_id, d.generation, due).unwrap(),
            Claimed::Skip("inbox_read")
        );
        assert_eq!(l.task(&id).unwrap().status, Status::Canceled);
        assert_eq!(l.project(T0).unwrap().attempts_used, used, "costs nothing");
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
            let w = l.project(T0).unwrap().wake.unwrap();
            assert_eq!(w.consecutive_failures, failures);
            if let Some(due) = wake_due(&l) {
                now = due;
            }
        }
        let w = l.project(T0).unwrap().wake.unwrap();
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
        assert_eq!(l.project(T0).unwrap().wake, None);
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
        assert_eq!(l.project(T0).unwrap().attempts_remaining, 0);
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
        l.post(T0, |_| Ok(serde_json::json!({ "event": "test" })))
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
            l.project(T0).unwrap().wake.unwrap().consecutive_failures,
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
}
