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
        let spec = request.validate(now).map_err(ApiError::invalid_cron)?;
        self.check_secrets(&spec.request.headers, spec.request.destination.host())?;
        let existing = self.cron_row(&spec.name)?;
        if existing.is_none() {
            let count: Vec<Counts> = self
                .db
                .query("SELECT COUNT(*) AS total, 0 AS waiting FROM crons", vec![])?;
            if count.first().map_or(0, |c| c.total) >= MAX_CRONS {
                return Err(ApiError::new(
                    429,
                    "cron_limit",
                    format!("this project already has {MAX_CRONS} crons; delete one first"),
                ));
            }
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
    pub fn fire_crons(&self, now: i64) -> Result<usize, DbError> {
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
