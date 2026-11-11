//! Crons: the `crons` table, the runs it starts, and each cron's health. The rest of the
//! ledger tells it when a run finishes, asks when the next run is due, and restarts crons
//! stopped by expiry; it never reads the table itself.

use super::*;
use runlater_core::HealthAlert;

/// Header values and the body are never echoed back, as for tasks.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct CronView {
    pub name: String,
    pub schedule: String,
    pub method: Method,
    pub url: String,
    pub header_names: Vec<String>,
    pub body_bytes: usize,
    pub max_attempts: u32,
    pub alert_url: Option<String>,
    /// Where each run reports when it finishes (a URL or "inbox"), and which runs: always,
    /// or failures only.
    pub callback_url: Option<String>,
    pub report: Report,
    /// Whether each run keeps the destination's response.
    pub capture: bool,
    /// None once the cron has stopped (the trial ended).
    pub next_run_ms: Option<i64>,
    pub health: Health,
    /// Why it is failing: run_failed, budget_exhausted, task_limit or trial_expired.
    pub health_reason: Option<String>,
    pub last_run_task_id: Option<String>,
    pub last_run_ms: Option<i64>,
    pub last_success_ms: Option<i64>,
    pub failing_since_ms: Option<i64>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct CronList {
    pub crons: Vec<CronView>,
}

#[derive(Deserialize)]
struct CronRow {
    name: String,
    schedule: String,
    method: String,
    url: String,
    headers: String,
    body: Option<String>,
    max_attempts: u32,
    alert_url: Option<String>,
    callback_url: Option<String>,
    capture: i64,
    report: String,
    next_run_ms: Option<i64>,
    health: String,
    health_reason: Option<String>,
    last_run_task_id: Option<String>,
    last_run_ms: Option<i64>,
    last_success_ms: Option<i64>,
    failing_since_ms: Option<i64>,
    /// Slot of the run (or missed run) that last set `health`; older news is ignored.
    health_run_at_ms: Option<i64>,
    created_ms: i64,
    updated_ms: i64,
}

const CRON_COLUMNS: &str = "name, schedule, method, url, headers, body, max_attempts, \
    alert_url, callback_url, capture, report, next_run_ms, health, health_reason, last_run_task_id, last_run_ms, \
    last_success_ms, failing_since_ms, health_run_at_ms, created_ms, updated_ms";

impl CronRow {
    /// Stored as text; anything unexpected reads as pending, as it always has.
    fn health(&self) -> Health {
        match self.health.as_str() {
            "healthy" => Health::Healthy,
            "failing" => Health::Failing,
            _ => Health::Pending,
        }
    }

    fn headers(&self) -> Result<Vec<(String, String)>, DbError> {
        serde_json::from_str(&self.headers).map_err(|e| DbError(e.to_string()))
    }

    fn view(&self) -> Result<CronView, DbError> {
        Ok(CronView {
            name: self.name.clone(),
            schedule: self.schedule.clone(),
            method: parse(&self.method)?,
            url: self.url.clone(),
            header_names: self.headers()?.into_iter().map(|(k, _)| k).collect(),
            body_bytes: self.body.as_ref().map_or(0, String::len),
            max_attempts: self.max_attempts,
            alert_url: self.alert_url.clone(),
            callback_url: self.callback_url.clone(),
            report: parse(&self.report)?,
            capture: self.capture != 0,
            next_run_ms: self.next_run_ms,
            health: self.health(),
            health_reason: self.health_reason.clone(),
            last_run_task_id: self.last_run_task_id.clone(),
            last_run_ms: self.last_run_ms,
            last_success_ms: self.last_success_ms,
            failing_since_ms: self.failing_since_ms,
            created_ms: self.created_ms,
            updated_ms: self.updated_ms,
        })
    }
}

fn no_cron() -> ApiError {
    ApiError::not_found("cron_not_found", "no cron with that name in this project")
}

impl<D: Db> Ledger<'_, D> {
    fn cron_row(&self, name: &str) -> Result<Option<CronRow>, DbError> {
        Ok(self
            .db
            .query(
                &format!("SELECT {CRON_COLUMNS} FROM crons WHERE name = ?"),
                vec![name.into()],
            )?
            .pop())
    }

    /// Creates the cron, or replaces the definition of the one with this name (its health
    /// and history are kept). Returns whether it was created.
    pub fn put_cron(&self, request: CronRequest, now: i64) -> Result<(CronView, bool), ApiError> {
        let project = self.project_row()?.ok_or_else(no_project)?;
        if now >= project.expires_ms {
            return Err(ApiError::new(
                403,
                "trial_expired",
                "this project's attempts have expired",
            ));
        }
        let spec = request.validate(now).map_err(ApiError::invalid)?;
        self.check_secrets(&spec.request.headers, spec.request.destination.host())?;
        let existing = self.cron_row(&spec.name)?;
        if existing.is_none() {
            self.room_for("crons", "cron_limit", MAX_CRONS)?;
        }
        let request = &spec.request;
        let headers =
            serde_json::to_string(&request.headers).map_err(|e| DbError(e.to_string()))?;
        self.db.exec(
            "INSERT INTO crons (name, schedule, method, url, headers, body, max_attempts, \
             alert_url, callback_url, capture, report, next_run_ms, health, created_ms, \
             updated_ms) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?) \
             ON CONFLICT (name) DO UPDATE SET schedule = excluded.schedule, \
             method = excluded.method, url = excluded.url, headers = excluded.headers, \
             body = excluded.body, max_attempts = excluded.max_attempts, \
             alert_url = excluded.alert_url, callback_url = excluded.callback_url, \
             capture = excluded.capture, report = excluded.report, \
             next_run_ms = excluded.next_run_ms, updated_ms = excluded.updated_ms",
            vec![
                spec.name.as_str().into(),
                spec.schedule_text.as_str().into(),
                request.method.as_str().into(),
                request.destination.as_str().into(),
                headers.into(),
                request.body.clone().into(),
                request.max_attempts.into(),
                spec.alert.as_ref().map(|a| a.as_str()).into(),
                request.callback.as_ref().map(|c| c.as_str()).into(),
                request.capture.into(),
                spec.report.as_str().into(),
                spec.schedule.next_after(now).into(),
                now.into(),
                now.into(),
            ],
        )?;
        Ok((self.cron(&spec.name)?, existing.is_none()))
    }

    pub fn cron(&self, name: &str) -> Result<CronView, ApiError> {
        Ok(self.cron_row(name)?.ok_or_else(no_cron)?.view()?)
    }

    pub fn crons(&self) -> Result<CronList, ApiError> {
        self.project_row()?.ok_or_else(no_project)?;
        let rows: Vec<CronRow> = self.db.query(
            &format!("SELECT {CRON_COLUMNS} FROM crons ORDER BY name"),
            vec![],
        )?;
        let crons = rows.iter().map(CronRow::view).collect::<Result<_, _>>()?;
        Ok(CronList { crons })
    }

    /// Stops the cron. Runs already started finish normally.
    pub fn delete_cron(&self, name: &str) -> Result<CronView, ApiError> {
        let view = self.cron(name)?;
        self.db
            .exec("DELETE FROM crons WHERE name = ?", vec![name.into()])?;
        Ok(view)
    }

    /// Starts a run for every cron that is due. A run that cannot be admitted, or a trial
    /// that has ended, marks the cron failing (and alerts) instead. Missed slots are not
    /// made up: a cron runs at most once per call. Returns how many runs started.
    pub(super) fn fire_crons(&self, now: i64) -> Result<usize, DbError> {
        let Some(project) = self.project_row()? else {
            return Ok(0);
        };
        // Once the trial has ended, every cron that is still scheduled stops now.
        let until = if now >= project.expires_ms {
            MAX_SAFE_INTEGER
        } else {
            now
        };
        let due: Vec<CronRow> = self.db.query(
            &format!(
                "SELECT {CRON_COLUMNS} FROM crons WHERE next_run_ms IS NOT NULL AND next_run_ms <= ?"
            ),
            vec![until.into()],
        )?;
        let mut started = 0;
        for cron in &due {
            if now >= project.expires_ms {
                self.db.exec(
                    "UPDATE crons SET next_run_ms = NULL, updated_ms = ? WHERE name = ?",
                    vec![now.into(), cron.name.as_str().into()],
                )?;
                self.health_news(cron, Some("trial_expired"), None, now, now)?;
                continue;
            }
            let schedule = Schedule::parse(&cron.schedule).map_err(|e| DbError(e.to_string()))?;
            let slot = cron.next_run_ms.unwrap_or(now);
            self.db.exec(
                "UPDATE crons SET next_run_ms = ?, updated_ms = ? WHERE name = ?",
                vec![
                    schedule.next_after(now).into(),
                    now.into(),
                    cron.name.as_str().into(),
                ],
            )?;
            if let Err(e) = self.admit(&project, Cost::task(cron.callback_url.as_deref()), now) {
                if e.status >= 500 {
                    return Err(DbError(e.message));
                }
                self.health_news(cron, Some(e.code), None, slot, now)?;
                continue;
            }
            let id = derived_id(&format!("cron/{}/{slot}", cron.name));
            let headers = cron.headers()?;
            self.insert(
                NewTask {
                    // The slot it fills, not when the alarm got to it: that difference is
                    // lateness.
                    run_at_ms: slot,
                    reporting: Reporting {
                        to: cron.callback_url.as_deref(),
                        failures_only: cron.report == Report::Failures.as_str(),
                        capture: cron.capture != 0,
                    },
                    ..NewTask::internal(
                        &id,
                        Kind::CronRun { cron: &cron.name },
                        Task::new(now, cron.max_attempts),
                        parse(&cron.method)?,
                        &cron.url,
                        &headers,
                        cron.body.clone(),
                    )
                },
                now,
            )?;
            self.db.exec(
                "UPDATE crons SET last_run_task_id = ?, last_run_ms = ? WHERE name = ?",
                vec![id.as_str().into(), now.into(), cron.name.as_str().into()],
            )?;
            started += 1;
        }
        Ok(started)
    }

    /// A run reached a final state: update its cron's health. Canceled runs say nothing,
    /// nor do runs of a cron that has since been deleted (even if re-created by name), nor
    /// runs older than the one that last set the cron's health.
    pub(super) fn run_finished(
        &self,
        name: &str,
        run: &TaskRow,
        status: Status,
        now: i64,
    ) -> Result<(), DbError> {
        let Some(cron) = self.cron_row(name)? else {
            return Ok(());
        };
        if run.created_ms <= cron.created_ms {
            return Ok(());
        }
        match status {
            Status::Succeeded => self.health_news(&cron, None, Some(&run.id), run.run_at_ms, now),
            Status::Failed => {
                self.health_news(&cron, Some("run_failed"), Some(&run.id), run.run_at_ms, now)
            }
            _ => Ok(()),
        }
    }

    /// Records news about the run for `slot`: it succeeded (`failure` None) or failed for
    /// that reason. Alerts once when the cron turns failing and once when it recovers.
    fn health_news(
        &self,
        cron: &CronRow,
        failure: Option<&str>,
        task_id: Option<&str>,
        slot: i64,
        now: i64,
    ) -> Result<(), DbError> {
        let Some(change) = cron
            .health()
            .after(cron.health_run_at_ms, slot, failure.is_none())
        else {
            return Ok(());
        };
        let name = cron.name.as_str();
        match (failure, change.alert) {
            (None, _) => self.db.exec(
                "UPDATE crons SET health = ?, health_reason = NULL, failing_since_ms = NULL, \
                 last_success_ms = ?, health_run_at_ms = ?, updated_ms = ? WHERE name = ?",
                vec![
                    text(&change.health).into(),
                    now.into(),
                    slot.into(),
                    now.into(),
                    name.into(),
                ],
            )?,
            (Some(reason), Some(HealthAlert::Failing)) => self.db.exec(
                "UPDATE crons SET health = ?, health_reason = ?, failing_since_ms = ?, \
                 health_run_at_ms = ?, updated_ms = ? WHERE name = ?",
                vec![
                    text(&change.health).into(),
                    reason.into(),
                    now.into(),
                    slot.into(),
                    now.into(),
                    name.into(),
                ],
            )?,
            (Some(reason), _) => self.db.exec(
                "UPDATE crons SET health_reason = ?, health_run_at_ms = ?, updated_ms = ? \
                 WHERE name = ?",
                vec![reason.into(), slot.into(), now.into(), name.into()],
            )?,
        }
        match change.alert {
            // queue_alert reads the cron again, so the alert shows the health just set.
            Some(alert) => self.queue_alert(name, alert.event(), failure, task_id, now),
            None => Ok(()),
        }
    }

    /// Alerts are ordinary tasks, like callbacks: signed, retried, one try past the trial.
    fn queue_alert(
        &self,
        name: &str,
        event: &str,
        reason: Option<&str>,
        task_id: Option<&str>,
        now: i64,
    ) -> Result<(), DbError> {
        let Some(cron) = self.cron_row(name)? else {
            return Ok(());
        };
        let Some(url) = &cron.alert_url else {
            return Ok(());
        };
        let report = serde_json::json!({
            "event": event,
            "reason": reason,
            "task_id": task_id,
            "cron": cron.view()?,
        });
        if url == INBOX {
            return self.post(now, |_| Ok(report)).map(|_| ());
        }
        let id = derived_id(&format!(
            "alert/{name}/{event}/{now}/{}",
            task_id.unwrap_or("-")
        ));
        self.insert(
            NewTask::internal(
                &id,
                Kind::Alert { cron: name },
                Task::new(now, DEFAULT_MAX_ATTEMPTS),
                Method::Post,
                url,
                &[("Content-Type".into(), "application/json".into())],
                Some(report.to_string()),
            ),
            now,
        )
    }

    /// Crons stopped when the project's attempts expired start again from now.
    pub(super) fn restart_stopped_crons(&self, now: i64) -> Result<(), DbError> {
        let stopped: Vec<CronRow> = self.db.query(
            &format!("SELECT {CRON_COLUMNS} FROM crons WHERE next_run_ms IS NULL"),
            vec![],
        )?;
        for cron in stopped {
            // Schedules were validated when stored; one that no longer parses stays stopped
            // rather than failing a payment that has already been applied.
            let Ok(schedule) = Schedule::parse(&cron.schedule) else {
                continue;
            };
            self.db.exec(
                "UPDATE crons SET next_run_ms = ?, updated_ms = ? WHERE name = ?",
                vec![
                    schedule.next_after(now).into(),
                    now.into(),
                    cron.name.as_str().into(),
                ],
            )?;
        }
        Ok(())
    }

    /// When the next cron run is due; a cron due after the project expires is due then, to stop.
    pub(super) fn crons_next_due(&self) -> Result<Option<i64>, DbError> {
        let rows: Vec<Due> = self.db.query(
            "SELECT MIN(MIN(next_run_ms, COALESCE((SELECT expires_ms FROM project), next_run_ms))) \
             AS t FROM crons WHERE next_run_ms IS NOT NULL",
            vec![],
        )?;
        Ok(rows.first().and_then(|d| d.t))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::Memory;
    use crate::ledger::testing::*;

    #[test]
    fn a_cron_run_is_late_from_its_slot() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(daily("backup"), T0).unwrap();
        // An alarm an hour late is real lateness, even though the task was due when created.
        assert_eq!(l.fire_crons(SIX + 3_600_000).unwrap(), 1);
        let id = l.cron("backup").unwrap().last_run_task_id.unwrap();
        let d = l
            .publish_due(P, SIX + 3_600_000)
            .unwrap()
            .into_iter()
            .find(|d| d.task_id == id)
            .unwrap();
        assert_eq!(deliver(&l, &d, SIX + 3_600_040).late_ms, Some(3_600_040));
    }

    fn run_cron_task(l: &Ledger<'_, Memory>, name: &str, status: u16, now: i64) -> String {
        let id = l.cron(name).unwrap().last_run_task_id.unwrap();
        drive(l, &id, http(status), None, now);
        id
    }

    #[test]
    fn a_cron_runs_on_schedule_as_ordinary_tasks() {
        let db = Memory::new();
        let l = setup(&db);
        let (c, created) = l.put_cron(daily("backup"), T0).unwrap();
        assert!(created);
        assert_eq!((c.next_run_ms, c.health), (Some(SIX), Health::Pending));
        assert_eq!(l.next_wake().unwrap(), Some(SIX));

        assert_eq!(l.fire_crons(SIX - 1).unwrap(), 0);
        assert_eq!(l.fire_crons(SIX).unwrap(), 1);
        assert_eq!(l.fire_crons(SIX).unwrap(), 0, "once per slot");
        let c = l.cron("backup").unwrap();
        assert_eq!(c.next_run_ms, Some(SIX + DAY));
        let run = l.task(c.last_run_task_id.as_deref().unwrap()).unwrap();
        assert_eq!(run.cron.as_deref(), Some("backup"));
        assert_eq!(
            (run.url.as_str(), run.run_at_ms),
            ("https://a.example.com/backup", SIX)
        );

        run_cron_task(&l, "backup", 200, SIX + 5);
        let c = l.cron("backup").unwrap();
        assert_eq!(
            (c.health, c.last_success_ms),
            (Health::Healthy, Some(SIX + 5))
        );
        assert!(alerts(&db, "backup").is_empty(), "no news is good news");
    }

    #[test]
    fn alerts_when_a_cron_starts_failing_and_when_it_recovers() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(daily("backup"), T0).unwrap();
        l.fire_crons(SIX).unwrap();
        let failed_run = run_cron_task(&l, "backup", 500, SIX);
        let c = l.cron("backup").unwrap();
        assert_eq!(c.health, Health::Failing);
        assert_eq!(c.health_reason.as_deref(), Some("run_failed"));
        assert_eq!(c.failing_since_ms, Some(SIX));

        // Failing again does not alert again.
        l.fire_crons(SIX + DAY).unwrap();
        run_cron_task(&l, "backup", 500, SIX + DAY);
        l.fire_crons(SIX + 2 * DAY).unwrap();
        run_cron_task(&l, "backup", 200, SIX + 2 * DAY);

        let sent = alerts(&db, "backup");
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0]["event"], "cron.failing");
        assert_eq!(sent[0]["reason"], "run_failed");
        assert_eq!(sent[0]["task_id"], failed_run.as_str());
        assert_eq!(sent[0]["cron"]["health"], "failing");
        assert_eq!(sent[1]["event"], "cron.recovered");
        assert_eq!(sent[1]["cron"]["health"], "healthy");

        let alert_id: String =
            db.0.query_row(
                "SELECT id FROM tasks WHERE alert_for = 'backup' ORDER BY seq LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let alert = l.task(&alert_id).unwrap();
        assert_eq!(alert.url, "https://alerts.example.com/hook");
        assert_eq!(alert.alert_for.as_deref(), Some("backup"));
    }

    #[test]
    fn missed_slots_are_not_made_up() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(daily("backup"), T0).unwrap();
        assert_eq!(l.fire_crons(SIX + 3 * DAY + 1).unwrap(), 1);
        assert_eq!(l.cron("backup").unwrap().next_run_ms, Some(SIX + 4 * DAY));
    }

    #[test]
    fn a_run_that_cannot_start_is_a_failure() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(daily("backup"), T0).unwrap();
        db.0.execute("UPDATE project SET attempts_used = attempt_budget", [])
            .unwrap();
        assert_eq!(l.fire_crons(SIX).unwrap(), 0);
        let c = l.cron("backup").unwrap();
        assert_eq!(c.health_reason.as_deref(), Some("budget_exhausted"));
        assert_eq!(c.last_run_task_id, None);
        assert_eq!(c.next_run_ms, Some(SIX + DAY), "keeps trying on schedule");
        // The alert still gets its one try.
        let sent = alerts(&db, "backup");
        assert_eq!(sent[0]["reason"], "budget_exhausted");
        let d = &l.publish_due(P, SIX).unwrap()[0];
        assert!(matches!(
            l.claim(&d.task_id, d.generation, SIX).unwrap(),
            Claimed::Deliver(_)
        ));
    }

    #[test]
    fn a_cron_stops_when_the_trial_ends() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(daily("backup"), T0).unwrap();
        let end = T0 + TRIAL_DURATION_MS;
        l.fire_crons(end).unwrap();
        let c = l.cron("backup").unwrap();
        assert_eq!(c.next_run_ms, None);
        assert_eq!(c.health_reason.as_deref(), Some("trial_expired"));
        assert_eq!(alerts(&db, "backup").len(), 1);
        assert_eq!(
            l.put_cron(daily("other"), end).unwrap_err().code,
            "trial_expired"
        );
    }

    #[test]
    fn replaces_lists_and_deletes_crons() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(daily("b"), T0).unwrap();
        l.put_cron(daily("a"), T0).unwrap();
        let hourly = CronRequest {
            schedule: "0 * * * *".into(),
            ..daily("b")
        };
        let (c, created) = l.put_cron(hourly, T0 + 1).unwrap();
        assert!(!created);
        assert_eq!(
            (c.schedule.as_str(), c.next_run_ms),
            ("0 * * * *", Some(T0 + 3_600_000))
        );
        let names: Vec<_> = l
            .crons()
            .unwrap()
            .crons
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["a", "b"]);

        l.delete_cron("b").unwrap();
        assert_eq!(l.cron("b").unwrap_err().code, "cron_not_found");
        assert_eq!(l.delete_cron("b").unwrap_err().status, 404);
        assert_eq!(l.next_wake().unwrap(), Some(SIX));

        let err = l
            .put_cron(
                CronRequest {
                    schedule: "*/5 * * * *".into(),
                    ..daily("c")
                },
                T0,
            )
            .unwrap_err();
        assert_eq!((err.status, err.field), (400, Some("schedule")));
        for i in 1..MAX_CRONS {
            l.put_cron(daily(&format!("n{i}")), T0).unwrap();
        }
        assert_eq!(
            l.put_cron(daily("full"), T0).unwrap_err().code,
            "cron_limit"
        );
        l.put_cron(daily("a"), T0).expect("replacing is not adding");
    }

    #[test]
    fn a_deleted_crons_run_still_finishes_quietly() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(daily("backup"), T0).unwrap();
        l.fire_crons(SIX).unwrap();
        let id = l.cron("backup").unwrap().last_run_task_id.unwrap();
        l.delete_cron("backup").unwrap();
        let d = &l.publish_due(P, SIX).unwrap()[0];
        let a = deliver(&l, d, SIX);
        l.finish(&id, a.fence, http(500), SIX, 0.0).unwrap();
        assert_eq!(l.task(&id).unwrap().status, Status::Failed);
        assert!(alerts(&db, "backup").is_empty());
    }

    #[test]
    fn a_recreated_cron_ignores_its_predecessors_runs() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(daily("backup"), T0).unwrap();
        l.fire_crons(SIX).unwrap();
        let old = l.cron("backup").unwrap().last_run_task_id.unwrap();
        l.delete_cron("backup").unwrap();
        l.put_cron(daily("backup"), SIX + 1).unwrap();
        let d = &l.publish_due(P, SIX + 2).unwrap()[0];
        assert_eq!(d.task_id, old);
        let a = deliver(&l, d, SIX + 2);
        l.finish(&old, a.fence, http(500), SIX + 2, 0.0).unwrap();
        assert_eq!(l.cron("backup").unwrap().health, Health::Pending);
        assert!(alerts(&db, "backup").is_empty());
    }

    #[test]
    fn an_older_run_cannot_overrule_a_newer_one() {
        let db = Memory::new();
        let l = setup(&db);
        let hourly = CronRequest {
            schedule: "0 * * * *".into(),
            max_attempts: Some(2),
            ..daily("h")
        };
        l.put_cron(hourly, T0).unwrap();
        let hour = 3_600_000;
        l.fire_crons(T0 + hour).unwrap();
        let first = l.cron("h").unwrap().last_run_task_id.unwrap();
        l.fire_crons(T0 + 2 * hour).unwrap();
        let second = l.cron("h").unwrap().last_run_task_id.unwrap();
        let due = l.publish_due(P, T0 + 2 * hour).unwrap();
        let attempt = |id: &str| {
            let d = due.iter().find(|d| d.task_id == id).unwrap();
            deliver(&l, d, T0 + 2 * hour)
        };
        let (a1, a2) = (attempt(&first), attempt(&second));
        // The newer run succeeds first; the older one failing afterwards is old news.
        l.finish(&second, a2.fence, http(200), T0 + 2 * hour + 1, 0.0)
            .unwrap();
        l.finish(&first, a1.fence, http(404), T0 + 2 * hour + 2, 0.0)
            .unwrap();
        assert_eq!(l.cron("h").unwrap().health, Health::Healthy);
        assert!(alerts(&db, "h").is_empty());
    }

    #[test]
    fn the_trial_ending_stops_crons_promptly() {
        let db = Memory::new();
        let l = setup(&db);
        let yearly = CronRequest {
            schedule: "0 0 1 1 *".into(),
            ..daily("yearly")
        };
        l.put_cron(yearly, T0).unwrap();
        let end = T0 + TRIAL_DURATION_MS;
        assert_eq!(
            l.next_wake().unwrap(),
            Some(end),
            "wakes at the trial's end"
        );
        l.fire_crons(end).unwrap();
        let c = l.cron("yearly").unwrap();
        assert_eq!(
            (c.next_run_ms, c.health_reason.as_deref()),
            (None, Some("trial_expired"))
        );
        assert_eq!(l.next_wake().unwrap(), alerts_wake(&db));
    }

    /// The only thing left to wake for: the alert just queued.
    fn alerts_wake(db: &Memory) -> Option<i64> {
        db.0.query_row(
            "SELECT MIN(due_ms) FROM tasks WHERE alert_for IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn a_cron_can_alert_to_the_inbox() {
        let db = Memory::new();
        let l = setup(&db);
        let cron = CronRequest {
            alert_url: Some(INBOX.into()),
            ..daily("backup")
        };
        assert_eq!(
            l.put_cron(cron, T0).unwrap().0.alert_url.as_deref(),
            Some("inbox")
        );
        l.fire_crons(SIX).unwrap();
        let run = run_cron_task(&l, "backup", 500, SIX);
        assert_eq!(
            alerts(&db, "backup"),
            Vec::<serde_json::Value>::new(),
            "no alert task"
        );
        let e = &inbox(&l).events[0];
        assert_eq!(e["event"], "cron.failing");
        assert_eq!(e["reason"], "run_failed");
        assert_eq!(e["task_id"], run.as_str());
        assert_eq!(e["cron"]["name"], "backup");
    }

    fn reporting(name: &str, extra: serde_json::Value) -> CronRequest {
        let mut r = serde_json::json!({
            "name": name,
            "schedule": "0 6 * * *",
            "url": "https://a.example.com/status",
            "method": "GET",
            "max_attempts": 1,
            "callback_url": "inbox",
            "capture": true,
        });
        r.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(r).unwrap()
    }

    fn run_id(l: &Ledger<'_, Memory>, name: &str) -> String {
        l.cron(name).unwrap().last_run_task_id.unwrap()
    }

    #[test]
    fn a_cron_run_reports_to_the_inbox_with_its_response() {
        let db = Memory::new();
        let l = setup(&db);
        let (c, _) = l
            .put_cron(reporting("health", serde_json::json!({})), T0)
            .unwrap();
        assert_eq!(
            (c.callback_url.as_deref(), c.report, c.capture),
            (Some("inbox"), Report::Always, true)
        );
        l.fire_crons(SIX).unwrap();
        let id = run_id(&l, "health");
        let t = l.task(&id).unwrap();
        assert_eq!(
            (t.callback_url.as_deref(), t.capture),
            (Some("inbox"), true)
        );
        answer(&l, &id, 200, r#"{"deploy":"failed"}"#, &[], SIX);
        let e = &inbox(&l).events[0];
        assert_eq!(e["event"], "task.finished");
        assert_eq!(e["task"]["cron"], "health");
        assert_eq!(e["task"]["response"]["body"], r#"{"deploy":"failed"}"#);
        // The next day's run reports again.
        l.fire_crons(SIX + DAY).unwrap();
        answer(&l, &run_id(&l, "health"), 200, "{}", &[], SIX + DAY);
        assert_eq!(inbox(&l).events.len(), 2);
    }

    #[test]
    fn report_failures_keeps_a_healthy_cron_quiet() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(
            reporting("health", serde_json::json!({ "report": "failures" })),
            T0,
        )
        .unwrap();
        l.set_wake(waking(serde_json::json!({})), T0).unwrap();
        l.fire_crons(SIX).unwrap();
        answer(&l, &run_id(&l, "health"), 200, "fine", &[], SIX);
        assert_eq!(inbox(&l).events.len(), 0, "a success says nothing");
        assert_eq!(wake_tasks(&db), 0, "and wakes nobody");

        l.fire_crons(SIX + DAY).unwrap();
        let failed = run_id(&l, "health");
        answer(&l, &failed, 500, "broken", &[], SIX + DAY);
        let e = &inbox(&l).events[0];
        assert_eq!(
            (e["task"]["id"].as_str(), e["task"]["status"].as_str()),
            (Some(failed.as_str()), Some("failed"))
        );
        assert_eq!(e["task"]["response"]["body"], "broken");
        assert_eq!(wake_tasks(&db), 1, "the failure wakes the agent");
    }

    #[test]
    fn a_cron_reporting_to_a_url_reserves_its_callback() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(
            reporting(
                "health",
                serde_json::json!({ "callback_url": "https://cb.example.com/runs" }),
            ),
            T0,
        )
        .unwrap();
        db.0.execute("UPDATE project SET attempt_budget = 1", [])
            .unwrap();
        l.fire_crons(SIX).unwrap();
        let c = l.cron("health").unwrap();
        assert_eq!(
            (c.last_run_task_id, c.health_reason.as_deref()),
            (None, Some("budget_exhausted")),
            "a run and its callback need two attempts"
        );

        db.0.execute("UPDATE project SET attempt_budget = 10", [])
            .unwrap();
        l.fire_crons(SIX + DAY).unwrap();
        let id = run_id(&l, "health");
        answer(&l, &id, 200, "ok", &[], SIX + DAY);
        let cb = l
            .task(&id)
            .unwrap()
            .callback_task_id
            .expect("a callback task");
        assert_eq!(l.task(&cb).unwrap().url, "https://cb.example.com/runs");
    }

    #[test]
    fn replacing_a_cron_changes_how_it_reports_and_old_crons_stay_quiet() {
        let db = Memory::new();
        let l = setup(&db);
        let (c, _) = l.put_cron(daily("backup"), T0).unwrap();
        assert_eq!(
            (c.callback_url, c.report, c.capture),
            (None, Report::Always, false)
        );
        let (c, created) = l
            .put_cron(
                reporting("backup", serde_json::json!({ "report": "failures" })),
                T0,
            )
            .unwrap();
        assert!(!created);
        assert_eq!(
            (c.callback_url.as_deref(), c.report),
            (Some("inbox"), Report::Failures)
        );
        let e = l
            .put_cron(
                CronRequest {
                    report: Some(Report::Failures),
                    ..daily("x")
                },
                T0,
            )
            .unwrap_err();
        assert_eq!((e.status, e.field), (400, Some("report")));
    }

    #[test]
    fn a_failures_only_run_reports_when_abandoned_but_not_when_canceled() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(
            reporting("health", serde_json::json!({ "report": "failures" })),
            T0,
        )
        .unwrap();
        l.fire_crons(SIX).unwrap();
        let canceled = run_id(&l, "health");
        let t = l.cancel(&canceled, SIX).unwrap();
        assert!(t.report_failures_only);
        assert_eq!(inbox(&l).events.len(), 0, "a canceled run did not fail");

        // Started, then out of attempts before it could be sent: that run failed, so it reports.
        l.fire_crons(SIX + DAY).unwrap();
        let starved = run_id(&l, "health");
        db.0.execute("UPDATE project SET attempts_used = attempt_budget", [])
            .unwrap();
        let d = l
            .publish_due(P, SIX + DAY)
            .unwrap()
            .into_iter()
            .find(|d| d.task_id == starved)
            .unwrap();
        assert_eq!(
            l.claim(&d.task_id, d.generation, SIX + DAY).unwrap(),
            Claimed::Skip("budget_exhausted")
        );
        let e = &inbox(&l).events[0];
        assert_eq!(
            (e["task"]["id"].as_str(), e["task"]["status"].as_str()),
            (Some(starved.as_str()), Some("failed"))
        );
    }
}
