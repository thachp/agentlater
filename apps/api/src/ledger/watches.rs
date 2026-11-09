//! Watches: tasks whose attempts are polls, and the action a match sends. The rest of the
//! ledger hands a finished poll to [`Ledger::finish_poll`], tells a watch that gave up before
//! polling, and tells it when it ended. A watch's progress (`watch_errors`, `watch_state`) is
//! read and written only here; `tasks.rs` stores its settings and action as the kind's data.

use super::*;

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct WatchView {
    pub every_ms: i64,
    pub deadline_ms: i64,
    pub until: Until,
    /// Polls in a row that failed (5xx, timeout, network); three end the watch.
    pub consecutive_errors: u32,
    /// Why the watch ended: matched, timed_out, poll_failing, poll_rejected,
    /// response_too_large, budget_exhausted, or a secret failure (secret_missing,
    /// secret_host_not_allowed, secret_too_large, secret_unavailable). None while it is still
    /// polling, or if canceled.
    pub reason: Option<String>,
    /// The value that satisfied `until`.
    pub matched: Option<serde_json::Value>,
    /// The last poll's response status, and up to 16 KiB of its body.
    pub last_status: Option<u16>,
    pub last_body: Option<String>,
    /// The request to send when `until` matches (header values and body not shown).
    pub on_match: Option<OnMatchView>,
    /// The task `on_match` queued, once the watch matched.
    pub action_task_id: Option<String>,
    /// Why a watch that matched sent no action: `canceled` (cancel was asked for while the
    /// matching poll was out).
    pub action_skipped: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct OnMatchView {
    pub method: Method,
    pub url: String,
    pub header_names: Vec<String>,
    pub callback_url: Option<String>,
    pub capture: bool,
}

/// A watch's `on_match` as stored in `tasks.on_match`. Header values and the body are
/// cleared once the watch ends, as the watch's own are.
#[derive(Debug, Serialize, Deserialize)]
struct StoredAction {
    method: Method,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
    max_attempts: u32,
    callback_url: Option<String>,
    capture: bool,
}

/// A watch's progress as stored in `tasks.watch_state`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct WatchState {
    reason: Option<String>,
    matched: Option<serde_json::Value>,
    last_status: Option<u16>,
    last_body: Option<String>,
    action_task_id: Option<String>,
    action_skipped: Option<String>,
}

impl TaskRow {
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
    pub(super) fn watch_view(&self, full: bool) -> Result<Option<WatchView>, DbError> {
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
            on_match: self.on_match()?.map(|a| OnMatchView {
                method: a.method,
                url: a.url,
                header_names: a.headers.into_iter().map(|(k, _)| k).collect(),
                callback_url: a.callback_url,
                capture: a.capture,
            }),
            action_task_id: state.action_task_id,
            action_skipped: state.action_skipped,
        }))
    }

    fn on_match(&self) -> Result<Option<StoredAction>, DbError> {
        self.on_match
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|e| DbError(e.to_string()))
    }
}

impl<D: Db> Ledger<'_, D> {
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
        if let Some(action) = &spec.on_match {
            self.check_secrets(&action.headers, action.destination.host())
                .map_err(|e| ApiError {
                    field: Some("on_match"),
                    ..e
                })?;
        }
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
        // The action, and its callback, are reserved now: a watch that matches always gets to
        // send it, and one that can't afford it is refused here rather than hours later.
        let action = spec
            .on_match
            .as_ref()
            .map(|a| Cost::task(a.callback.as_ref().map(|c| c.as_str())))
            .unwrap_or_default();
        let cost = Cost::task(request.callback.as_ref().map(|c| c.as_str())) + action;
        self.admit(&project, cost, now)?;
        let action_reserve = action.reserve();
        let watch = serde_json::to_string(&spec.watch).map_err(|e| DbError(e.to_string()))?;
        let on_match = spec
            .on_match
            .map(|a| {
                serde_json::to_string(&StoredAction {
                    method: a.method,
                    url: a.destination.as_str().into(),
                    headers: a.headers,
                    body: a.body,
                    max_attempts: a.max_attempts,
                    callback_url: a.callback.as_ref().map(|c| c.as_str().into()),
                    capture: a.capture,
                })
            })
            .transpose()
            .map_err(|e| DbError(e.to_string()))?;
        self.insert(
            NewTask {
                id: task_id,
                kind: Kind::Watch {
                    settings: &watch,
                    on_match: on_match.as_deref(),
                },
                task: Task::new(request.run_at_ms, request.max_attempts),
                method: request.method,
                url: request.destination.as_str(),
                headers: &request.headers,
                body: request.body,
                run_at_ms: request.run_at_ms,
                idempotency_key: request.idempotency_key,
                fingerprint: &request.fingerprint,
                reporting: Reporting {
                    to: request.callback.as_ref().map(|c| c.as_str()),
                    ..Reporting::default()
                },
                action_reserve,
            },
            now,
        )?;
        Ok((self.task(task_id)?, true))
    }

    /// The watch stopped before a poll could be made (out of attempts, or a secret it needs is
    /// gone): record why. Other kinds of task have nothing to record.
    pub(super) fn watch_gave_up(
        &self,
        row: &TaskRow,
        kind: Kind,
        reason: &str,
    ) -> Result<(), DbError> {
        if !kind.is_watch() {
            return Ok(());
        }
        let state = WatchState {
            reason: Some(reason.into()),
            ..row.watch_state()?
        };
        self.store_watch(&row.id, row.watch_errors, &state)
    }

    /// Judges a watch's poll: done when it matches, over when polling keeps failing or the
    /// deadline passes, otherwise scheduled again. `None` if the poll no longer holds the task.
    pub(super) fn finish_poll(
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

    /// A watch with `on_match` ended. If it matched, its action is queued, in the same
    /// transaction, under an id derived from the watch's (a watch matches once, so queueing
    /// it again is a no-op). Either way the action's header values and body are cleared.
    pub(super) fn watch_ended(&self, row: &TaskRow, task: &Task, now: i64) -> Result<(), DbError> {
        let action = row
            .on_match()?
            .ok_or_else(|| DbError(format!("task {} has no action", row.id)))?;
        if task.status == Status::Succeeded {
            // The poll just stored its verdict; build on that, not on `row`.
            let current = self
                .task_row(&row.id)?
                .ok_or_else(|| DbError(format!("task {} vanished", row.id)))?;
            let mut state = current.watch_state()?;
            if task.cancel_requested {
                // Asked to stop while the matching poll was out: the match is recorded, but
                // the agent no longer wants what follows.
                state.action_skipped = Some("canceled".into());
            } else {
                let id = derived_id(&format!("action/{}", row.id));
                self.insert(
                    NewTask {
                        reporting: Reporting {
                            to: action.callback_url.as_deref(),
                            failures_only: false,
                            capture: action.capture,
                        },
                        ..NewTask::internal(
                            &id,
                            Kind::Action { watch_id: &row.id },
                            Task::new(now, action.max_attempts),
                            action.method,
                            &action.url,
                            &action.headers,
                            action.body.clone(),
                        )
                    },
                    now,
                )?;
                state.action_task_id = Some(id);
            }
            self.store_watch(&row.id, current.watch_errors, &state)?;
        }
        let cleared = StoredAction {
            headers: action
                .headers
                .into_iter()
                .map(|(name, _)| (name, String::new()))
                .collect(),
            body: None,
            ..action
        };
        let cleared = serde_json::to_string(&cleared).map_err(|e| DbError(e.to_string()))?;
        self.db.exec(
            "UPDATE tasks SET on_match = ? WHERE id = ?",
            vec![cleared.into(), row.id.as_str().into()],
        )
    }
}
