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
        let inbox: Vec<Counts> = self.db.query(
            "SELECT COUNT(*) AS total, COALESCE(SUM(seq <= (SELECT inbox_acked_seq FROM project) \
             OR json_extract(event, '$.event') = 'hook.received'), 0) AS waiting FROM inbox",
            vec![],
        )?;
        Ok(inbox
            .first()
            .is_none_or(|c| c.total < MAX_INBOX_EVENTS || c.waiting > 0))
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
        let held: Vec<Counts> = self
            .db
            .query("SELECT COUNT(*) AS total, 0 AS waiting FROM inbox", vec![])?;
        if held.first().map_or(0, |c| c.total) <= MAX_INBOX_EVENTS {
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
