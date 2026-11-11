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
        let spec = request.validate(now).map_err(ApiError::invalid)?;
        self.check_secrets(&spec.request.headers, spec.request.destination.host())?;
        if let Some(action) = &spec.on_match {
            self.check_secrets(&action.headers, action.destination.host())
                .map_err(|e| e.in_field("on_match"))?;
        }
        let request = spec.request;
        if let Some(existing) =
            self.replay(request.idempotency_key.as_deref(), &request.fingerprint)?
        {
            return Ok((existing, false));
        }
        if spec.watch.deadline_ms >= project.expires_ms {
            return Err(
                ApiError::bad_request("deadline is after this project's trial ends")
                    .in_field("deadline"),
            );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::Memory;
    use crate::ledger::testing::*;
    use runlater_core::limits::LEASE_MS;

    const MIN: i64 = 60_000;

    /// Runs the watch's due poll at `now` with this response.
    fn poll(
        l: &Ledger<'_, Memory>,
        id: &str,
        outcome: Outcome,
        body: Option<&str>,
        now: i64,
    ) -> Status {
        let read = body.map(|text| ResponseRead {
            body: Some(PollBody {
                text,
                truncated: false,
            }),
            headers: &[],
        });
        let (a, status) = drive(l, id, outcome, read, now);
        assert!(a.read_body, "polls read the response");
        status.expect("current poll")
    }

    fn watch_view(l: &Ledger<'_, Memory>, id: &str) -> WatchView {
        l.task(id).unwrap().watch.expect("a watch")
    }

    #[test]
    fn a_watch_polls_until_it_matches_then_calls_back() {
        let db = Memory::new();
        let l = setup(&db);
        let req = watching(serde_json::json!({ "callback_url": "https://cb.example.com/done" }));
        let (t, created) = l.watch(req, "w1", T0).unwrap();
        assert!(created);
        assert_eq!(
            (t.status, t.method, t.next_attempt_ms),
            (Status::Scheduled, Method::Get, Some(T0))
        );
        let w = t.watch.unwrap();
        assert_eq!(
            (w.every_ms, w.deadline_ms, w.reason),
            (5 * MIN, T0 + 60 * MIN, None)
        );

        let running = Some(r#"{"status":"running"}"#);
        assert_eq!(poll(&l, "w1", http(200), running, T0), Status::Scheduled);
        assert_eq!(l.task("w1").unwrap().next_attempt_ms, Some(T0 + 5 * MIN));
        assert_eq!(watch_view(&l, "w1").last_body.as_deref(), running);
        assert!(
            l.publish_due(P, T0 + 5 * MIN - 1).unwrap().is_empty(),
            "waits for the next poll"
        );
        assert_eq!(
            poll(&l, "w1", http(200), running, T0 + 5 * MIN),
            Status::Scheduled
        );

        let done = r#"{"status":"succeeded","output":"s3://bucket/out"}"#;
        assert_eq!(
            poll(&l, "w1", http(200), Some(done), T0 + 10 * MIN),
            Status::Succeeded
        );
        let t = l.task("w1").unwrap();
        assert_eq!(t.attempt_count, 3);
        let w = t.watch.unwrap();
        assert_eq!(w.reason.as_deref(), Some("matched"));
        assert_eq!(w.matched, Some(serde_json::json!("succeeded")));
        assert_eq!(
            (w.last_status, w.last_body.as_deref()),
            (Some(200), Some(done))
        );
        assert_eq!(l.project().unwrap().attempts_used, 3);

        // The token is gone; the name stays.
        assert_eq!(t.header_names, ["Authorization"]);
        let stored: Vec<IdRow> = db
            .query("SELECT headers AS id FROM tasks WHERE id = 'w1'", vec![])
            .unwrap();
        assert!(!stored[0].id.contains("secret-token"), "{}", stored[0].id);

        let cb = l.task(t.callback_task_id.as_deref().unwrap()).unwrap();
        let d = l.publish_due(P, T0 + 10 * MIN).unwrap();
        assert_eq!(d[0].task_id, cb.id);
        let report: serde_json::Value =
            serde_json::from_str(deliver(&l, &d[0], T0 + 10 * MIN).body.as_deref().unwrap())
                .unwrap();
        assert_eq!(report["task"]["status"], "succeeded");
        assert_eq!(report["task"]["watch"]["matched"], "succeeded");
        assert_eq!(report["task"]["watch"]["last_body"], done);
    }

    #[test]
    fn lists_leave_out_the_last_response_body() {
        let db = Memory::new();
        let l = setup(&db);
        l.watch(watching(serde_json::json!({})), "w1", T0).unwrap();
        poll(&l, "w1", http(200), Some(r#"{"status":"running"}"#), T0);
        let listed = l
            .list(None, None, None, None)
            .unwrap()
            .tasks
            .remove(0)
            .watch
            .unwrap();
        assert_eq!((listed.last_status, listed.last_body), (Some(200), None));
        assert!(watch_view(&l, "w1").last_body.is_some(), "get_task has it");
    }

    #[test]
    fn three_failed_polls_in_a_row_end_a_watch() {
        let db = Memory::new();
        let l = setup(&db);
        l.watch(watching(serde_json::json!({})), "w1", T0).unwrap();
        assert_eq!(poll(&l, "w1", http(503), None, T0), Status::Scheduled);
        assert_eq!(
            poll(&l, "w1", Outcome::Timeout, None, T0 + 5 * MIN),
            Status::Scheduled
        );
        assert_eq!(watch_view(&l, "w1").consecutive_errors, 2);
        // A good answer resets the count.
        let running = Some(r#"{"status":"running"}"#);
        assert_eq!(
            poll(&l, "w1", http(200), running, T0 + 10 * MIN),
            Status::Scheduled
        );
        assert_eq!(watch_view(&l, "w1").consecutive_errors, 0);
        for n in 0..2 {
            let at = T0 + (15 + 5 * n) * MIN;
            assert_eq!(poll(&l, "w1", http(502), None, at), Status::Scheduled);
        }
        assert_eq!(
            poll(&l, "w1", Outcome::Network, None, T0 + 25 * MIN),
            Status::Failed
        );
        let w = watch_view(&l, "w1");
        assert_eq!(
            (w.reason.as_deref(), w.consecutive_errors),
            (Some("poll_failing"), 3)
        );
    }

    #[test]
    fn a_rejected_poll_ends_a_watch_at_once() {
        let db = Memory::new();
        let l = setup(&db);
        l.watch(watching(serde_json::json!({})), "w1", T0).unwrap();
        assert_eq!(
            poll(&l, "w1", http(401), Some("bad token"), T0),
            Status::Failed
        );
        let w = watch_view(&l, "w1");
        assert_eq!(w.reason.as_deref(), Some("poll_rejected"));
        assert_eq!(
            (w.last_status, w.last_body.as_deref()),
            (Some(401), Some("bad token"))
        );
    }

    #[test]
    fn a_watch_times_out_after_polling_at_its_deadline() {
        let db = Memory::new();
        let l = setup(&db);
        l.watch(
            watching(serde_json::json!({ "every": "5m", "deadline": "12m" })),
            "w1",
            T0,
        )
        .unwrap();
        let running = Some(r#"{"status":"running"}"#);
        for at in [T0, T0 + 5 * MIN, T0 + 10 * MIN] {
            assert_eq!(poll(&l, "w1", http(200), running, at), Status::Scheduled);
        }
        assert_eq!(l.task("w1").unwrap().next_attempt_ms, Some(T0 + 12 * MIN));
        assert_eq!(
            poll(&l, "w1", http(200), running, T0 + 12 * MIN),
            Status::Failed
        );
        assert_eq!(watch_view(&l, "w1").reason.as_deref(), Some("timed_out"));
        assert_eq!(l.task("w1").unwrap().attempt_count, 4);
    }

    #[test]
    fn a_lost_poll_counts_as_a_failed_one() {
        let db = Memory::new();
        let l = setup(&db);
        l.watch(watching(serde_json::json!({})), "w1", T0).unwrap();
        let d = &l.publish_due(P, T0).unwrap()[0];
        deliver(&l, d, T0);
        l.recover(T0 + LEASE_MS, || 0.0).unwrap();
        let t = l.task("w1").unwrap();
        assert_eq!(
            (t.status, t.next_attempt_ms),
            (Status::Scheduled, Some(T0 + LEASE_MS + 5 * MIN))
        );
        assert_eq!(watch_view(&l, "w1").consecutive_errors, 1);
        assert_eq!(
            t.attempts.unwrap()[0].outcome.as_deref(),
            Some("lease_expired")
        );
    }

    #[test]
    fn a_watch_can_be_canceled_but_not_redriven() {
        let db = Memory::new();
        let l = setup(&db);
        l.watch(watching(serde_json::json!({})), "w1", T0).unwrap();
        poll(&l, "w1", http(200), Some("{}"), T0);
        let t = l.cancel("w1", T0 + 1).unwrap();
        assert_eq!(t.status, Status::Canceled);
        assert_eq!(t.watch.unwrap().reason, None);

        l.watch(watching(serde_json::json!({})), "w2", T0).unwrap();
        poll(&l, "w2", http(404), None, T0);
        assert_eq!(
            l.redrive("w2", T0 + 1).unwrap_err().code,
            "watch_not_redrivable"
        );
    }

    #[test]
    fn a_watch_ends_when_the_trial_runs_out_of_attempts() {
        let db = Memory::new();
        let l = setup(&db);
        l.watch(watching(serde_json::json!({})), "w1", T0).unwrap();
        poll(&l, "w1", http(200), Some("{}"), T0);
        db.exec("UPDATE project SET attempts_used = attempt_budget", vec![])
            .unwrap();
        let d = &l.publish_due(P, T0 + 5 * MIN).unwrap()[0];
        assert_eq!(
            l.claim(&d.task_id, d.generation, T0 + 5 * MIN).unwrap(),
            Claimed::Skip("budget_exhausted")
        );
        let t = l.task("w1").unwrap();
        assert_eq!(t.status, Status::Failed);
        assert_eq!(t.watch.unwrap().reason.as_deref(), Some("budget_exhausted"));
    }

    #[test]
    fn watch_requests_are_checked() {
        let db = Memory::new();
        let l = setup(&db);
        let e = l
            .watch(watching(serde_json::json!({ "every": "1m" })), "w1", T0)
            .unwrap_err();
        assert_eq!((e.status, e.field), (400, Some("every")));
        let e = l
            .watch(
                watching(serde_json::json!({ "until": { "path": "/s" } })),
                "w1",
                T0,
            )
            .unwrap_err();
        assert_eq!(e.field, Some("until"));
        let e = l
            .watch(
                watching(serde_json::json!({ "every": "1d", "deadline": "7d" })),
                "w1",
                T0 + 1,
            )
            .unwrap_err();
        assert_eq!(e.field, Some("deadline"), "past the trial's end");

        let keyed = || watching(serde_json::json!({ "idempotency_key": "job-1" }));
        let (first, created) = l.watch(keyed(), "w1", T0).unwrap();
        assert!(created);
        let (again, created) = l.watch(keyed(), "w2", T0 + 1_000).unwrap();
        assert!(!created);
        assert_eq!(again.id, first.id);
        let changed = watching(serde_json::json!({ "idempotency_key": "job-1", "every": "10m" }));
        assert_eq!(
            l.watch(changed, "w3", T0).unwrap_err().code,
            "idempotency_conflict"
        );
    }

    #[test]
    fn a_watch_can_report_to_the_inbox() {
        let db = Memory::new();
        let l = setup(&db);
        l.watch(
            watching(serde_json::json!({ "callback_url": "inbox" })),
            "w1",
            T0,
        )
        .unwrap();
        poll(&l, "w1", http(200), Some(r#"{"status":"succeeded"}"#), T0);
        let e = &inbox(&l).events[0];
        assert_eq!(e["task"]["watch"]["reason"], "matched");
        assert_eq!(l.task("w1").unwrap().callback_task_id, None);
    }

    fn acting(extra: serde_json::Value) -> WatchRequest {
        let mut on_match = serde_json::json!({
            "url": "https://ci.example.com/smoke",
            "headers": { "Authorization": "Bearer ci-token" },
            "body": "{\"deployment\":123}",
        });
        if let Some(more) = extra.get("on_match") {
            for (k, x) in more.as_object().unwrap() {
                on_match[k] = x.clone();
            }
        }
        let mut rest = extra.clone();
        rest.as_object_mut().unwrap().remove("on_match");
        rest["on_match"] = on_match;
        watching(rest)
    }

    fn actions_of(db: &Memory, watch: &str) -> Vec<String> {
        let rows: Vec<IdRow> = db
            .query(
                "SELECT id FROM tasks WHERE triggered_by = ?",
                vec![watch.into()],
            )
            .unwrap();
        rows.into_iter().map(|r| r.id).collect()
    }

    const DONE: Option<&str> = Some(r#"{"status":"succeeded"}"#);
    const RUNNING: Option<&str> = Some(r#"{"status":"running"}"#);

    #[test]
    fn a_watch_that_matches_sends_its_action_once() {
        let db = Memory::new();
        let l = setup(&db);
        let req = acting(serde_json::json!({
            "callback_url": "inbox",
            "on_match": { "callback_url": "https://cb.example.com/smoke" },
        }));
        l.watch(req, "w1", T0).unwrap();
        let w = watch_view(&l, "w1");
        let on_match = w.on_match.unwrap();
        assert_eq!(
            (
                on_match.method,
                on_match.url.as_str(),
                on_match.header_names
            ),
            (
                Method::Post,
                "https://ci.example.com/smoke",
                vec!["Authorization".to_string()]
            )
        );
        assert_eq!(w.action_task_id, None);

        assert_eq!(poll(&l, "w1", http(503), None, T0), Status::Scheduled);
        assert!(actions_of(&db, "w1").is_empty(), "not until it matches");
        assert_eq!(
            poll(&l, "w1", http(200), DONE, T0 + 5 * MIN),
            Status::Succeeded
        );

        let w = watch_view(&l, "w1");
        assert_eq!(w.consecutive_errors, 0, "the match reset the count");
        let action_id = w.action_task_id.expect("action queued");
        assert_eq!(actions_of(&db, "w1"), std::slice::from_ref(&action_id));
        let a = l.task(&action_id).unwrap();
        assert_eq!(
            (
                a.status,
                a.method,
                a.url.as_str(),
                a.triggered_by.as_deref()
            ),
            (
                Status::Scheduled,
                Method::Post,
                "https://ci.example.com/smoke",
                Some("w1")
            )
        );
        assert_eq!(
            a.callback_url.as_deref(),
            Some("https://cb.example.com/smoke")
        );
        // The watch's report says what it set off.
        let e = &inbox(&l).events[0];
        assert_eq!(e["task"]["watch"]["action_task_id"], action_id.as_str());

        // The watch keeps no copy of the action's token or body; the action has them.
        let stored: Vec<IdRow> = db
            .query("SELECT on_match AS id FROM tasks WHERE id = 'w1'", vec![])
            .unwrap();
        assert!(!stored[0].id.contains("ci-token"), "{}", stored[0].id);
        assert!(!stored[0].id.contains("deployment"), "{}", stored[0].id);
        let d = l
            .publish_due(P, T0 + 5 * MIN)
            .unwrap()
            .into_iter()
            .find(|d| d.task_id == action_id)
            .expect("the action is due at once");
        let sent = deliver(&l, &d, T0 + 5 * MIN);
        assert_eq!(
            sent.headers,
            [("Authorization".to_string(), "Bearer ci-token".to_string())]
        );
        assert_eq!(sent.body.as_deref(), Some("{\"deployment\":123}"));
        assert_eq!(
            l.finish(&action_id, sent.fence, http(200), T0 + 5 * MIN, 0.0)
                .unwrap(),
            Some(Status::Succeeded)
        );
        let a = l.task(&action_id).unwrap();
        assert!(a.callback_task_id.is_some(), "reports like any task");
        assert_eq!(watch_view(&l, "w1").on_match.unwrap().url, a.url);
    }

    #[test]
    fn an_action_is_queued_at_most_once() {
        let db = Memory::new();
        let l = setup(&db);
        l.watch(acting(serde_json::json!({})), "w1", T0).unwrap();
        // A poll whose lease ran out comes back late with a match: it no longer holds the
        // watch, so it changes nothing.
        let d = l.publish_due(P, T0).unwrap().remove(0);
        let lost = deliver(&l, &d, T0);
        l.recover(T0 + LEASE_MS, || 0.0).unwrap();
        let read = |text| ResponseRead {
            body: Some(PollBody {
                text,
                truncated: false,
            }),
            headers: &[],
        };
        assert_eq!(
            l.finish_response(
                "w1",
                lost.fence,
                http(200),
                Some(read(DONE.unwrap())),
                T0 + LEASE_MS,
                0.0
            )
            .unwrap(),
            None
        );
        assert!(actions_of(&db, "w1").is_empty());

        let row = l.task_row("w1").unwrap().unwrap();
        let next = l.task("w1").unwrap().next_attempt_ms.unwrap();
        assert_eq!(poll(&l, "w1", http(200), DONE, next), Status::Succeeded);
        assert_eq!(actions_of(&db, "w1").len(), 1);
        // Finishing the same watch again queues under the same id, which is a no-op.
        let mut done = row.task().unwrap();
        done.status = Status::Succeeded;
        l.watch_ended(&row, &done, next + 1).unwrap();
        assert_eq!(actions_of(&db, "w1").len(), 1);
    }

    #[test]
    fn a_watch_that_does_not_match_sends_nothing() {
        let db = Memory::new();
        let l = setup(&db);
        let req = || acting(serde_json::json!({ "every": "5m", "deadline": "5m" }));
        l.watch(req(), "timed_out", T0).unwrap();
        poll(&l, "timed_out", http(200), RUNNING, T0);
        assert_eq!(
            poll(&l, "timed_out", http(200), RUNNING, T0 + 5 * MIN),
            Status::Failed
        );
        l.watch(req(), "rejected", T0).unwrap();
        assert_eq!(poll(&l, "rejected", http(404), None, T0), Status::Failed);
        l.watch(req(), "canceled", T0).unwrap();
        assert_eq!(l.cancel("canceled", T0).unwrap().status, Status::Canceled);
        for id in ["timed_out", "rejected", "canceled"] {
            assert!(actions_of(&db, id).is_empty(), "{id}");
            let w = watch_view(&l, id);
            assert_eq!((w.action_task_id, w.action_skipped), (None, None), "{id}");
            let stored: Vec<IdRow> = db
                .query(
                    "SELECT on_match AS id FROM tasks WHERE id = ?",
                    vec![id.into()],
                )
                .unwrap();
            assert!(!stored[0].id.contains("ci-token"), "{id}: {}", stored[0].id);
        }
    }

    #[test]
    fn canceling_during_the_matching_poll_skips_the_action() {
        let db = Memory::new();
        let l = setup(&db);
        l.watch(acting(serde_json::json!({})), "w1", T0).unwrap();
        let d = l.publish_due(P, T0).unwrap().remove(0);
        let a = deliver(&l, &d, T0);
        l.cancel("w1", T0 + 1).unwrap();
        let read = ResponseRead {
            body: Some(PollBody {
                text: DONE.unwrap(),
                truncated: false,
            }),
            headers: &[],
        };
        assert_eq!(
            l.finish_response("w1", a.fence, http(200), Some(read), T0 + 2, 0.0)
                .unwrap(),
            Some(Status::Succeeded)
        );
        let w = watch_view(&l, "w1");
        assert_eq!(w.reason.as_deref(), Some("matched"));
        assert_eq!(
            (w.action_task_id, w.action_skipped.as_deref()),
            (None, Some("canceled"))
        );
        assert!(actions_of(&db, "w1").is_empty());
    }

    #[test]
    fn admission_reserves_the_action_and_its_callback() {
        let db = Memory::new();
        let l = setup(&db);
        let left = |n: i64| {
            db.exec(
                "UPDATE project SET attempt_budget = attempts_used + ?",
                vec![n.into()],
            )
            .unwrap()
        };
        let with_callback = || {
            acting(serde_json::json!({ "on_match": { "callback_url": "https://cb.example.com/" } }))
        };
        left(2);
        let e = l.watch(with_callback(), "w0", T0).unwrap_err();
        assert_eq!((e.status, e.code), (402, "budget_exhausted"));
        // An inbox report is not a task, so it needs no attempt.
        let inboxed = acting(serde_json::json!({ "on_match": { "callback_url": "inbox" } }));
        l.watch(inboxed, "w1", T0).unwrap();
        let e = l
            .schedule(
                ScheduleRequest {
                    url: "https://api.example.com/other".into(),
                    ..Default::default()
                },
                "t1",
                T0,
            )
            .unwrap_err();
        assert_eq!(e.code, "budget_exhausted", "both attempts are promised");

        // The poll spends one; the action's is still there, even once the trial is spent.
        assert_eq!(poll(&l, "w1", http(200), DONE, T0), Status::Succeeded);
        db.exec(
            "UPDATE project SET attempts_used = attempt_budget + 5",
            vec![],
        )
        .unwrap();
        let id = watch_view(&l, "w1").action_task_id.unwrap();
        let d = l
            .publish_due(P, T0)
            .unwrap()
            .into_iter()
            .find(|d| d.task_id == id)
            .unwrap();
        let sent = deliver(&l, &d, T0);
        assert_eq!(
            l.finish(&id, sent.fence, http(503), T0, 0.0).unwrap(),
            Some(Status::Failed),
            "one try past the budget, no retries"
        );
        // A redrive is admitted like any task's, and earns no second free try.
        assert_eq!(l.redrive(&id, T0).unwrap_err().code, "budget_exhausted");
        db.exec(
            "UPDATE project SET attempt_budget = attempts_used + 1",
            vec![],
        )
        .unwrap();
        l.redrive(&id, T0).unwrap();
        db.exec("UPDATE project SET attempts_used = attempt_budget", vec![])
            .unwrap();
        let d = l
            .publish_due(P, T0)
            .unwrap()
            .into_iter()
            .find(|d| d.task_id == id)
            .unwrap();
        assert_eq!(
            l.claim(&d.task_id, d.generation, T0).unwrap(),
            Claimed::Skip("budget_exhausted")
        );
    }

    #[test]
    fn an_action_follows_secret_rules_for_its_own_host() {
        let db = Memory::new();
        let l = setup(&db);
        put_secret(&l, "CI", 20, &["ci.example.com"], T0).unwrap();
        put_secret(&l, "API", 20, &["api.example.com"], T0).unwrap();
        let with = |name: &str| {
            acting(serde_json::json!({
                "on_match": { "headers": { "Authorization": format!("{{{{secret:{name}}}}}") } }
            }))
        };
        // API's value may go to the watched host, but not to where the action sends it.
        let e = l.watch(with("API"), "w0", T0).unwrap_err();
        assert_eq!(
            (e.status, e.code, e.field),
            (400, "secret_host_not_allowed", Some("on_match"))
        );
        let e = l.watch(with("NOPE"), "w0", T0).unwrap_err();
        assert_eq!((e.code, e.field), ("secret_not_found", Some("on_match")));

        l.watch(with("CI"), "w1", T0).unwrap();
        poll(&l, "w1", http(200), DONE, T0);
        let id = watch_view(&l, "w1").action_task_id.unwrap();
        let d = l
            .publish_due(P, T0)
            .unwrap()
            .into_iter()
            .find(|d| d.task_id == id)
            .unwrap();
        let sent = deliver(&l, &d, T0);
        assert_eq!(sent.secrets, [sealed("CI", 20)]);
    }

    #[test]
    fn on_match_is_checked_when_the_watch_is_created() {
        let db = Memory::new();
        let l = setup(&db);
        let e = l
            .watch(
                acting(serde_json::json!({ "on_match": { "url": "http://ci.example.com/" } })),
                "w1",
                T0,
            )
            .unwrap_err();
        assert_eq!((e.status, e.field), (400, Some("on_match")));
        assert!(e.message.starts_with("on_match.url"), "{}", e.message);
    }
}
