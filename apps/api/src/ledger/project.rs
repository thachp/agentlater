//! The project: its trial, the attempts bought for it, and what its row lets it do.

use super::*;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectView {
    pub id: String,
    pub created_ms: i64,
    pub expires_ms: i64,
    pub attempt_budget: i64,
    pub attempts_used: i64,
    pub attempts_remaining: i64,
    /// Whether attempts have ever been bought for this project.
    pub paid: bool,
    /// Receivers verify `RunLater-Signature` with this. Only `create_project` returns it, so the
    /// routine reads of a project don't put it in every transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_secret: Option<String>,
    /// Inbox events not yet acked; `read_inbox` returns them.
    #[serde(default)]
    pub inbox_unread: i64,
    /// Where AgentLater sends a wake when the inbox gets something; see `set_wake`.
    #[serde(default)]
    pub wake: Option<WakeView>,
}

#[derive(Deserialize)]
pub(super) struct ProjectRow {
    pub(super) id: String,
    created_ms: i64,
    pub(super) expires_ms: i64,
    pub(super) attempt_budget: i64,
    pub(super) attempts_used: i64,
    pub(super) signing_secret: String,
    pub(super) paid: i64,
    pub(super) inbox_last_seq: i64,
    pub(super) inbox_acked_seq: i64,
    pub(super) inbox_dropped: i64,
    pub(super) inbox_unread: i64,
}

impl ProjectRow {
    fn view(self) -> ProjectView {
        ProjectView {
            attempts_remaining: (self.attempt_budget - self.attempts_used).max(0),
            id: self.id,
            created_ms: self.created_ms,
            expires_ms: self.expires_ms,
            attempt_budget: self.attempt_budget,
            attempts_used: self.attempts_used,
            paid: self.paid != 0,
            signing_secret: None,
            inbox_unread: self.inbox_unread,
            wake: None,
        }
    }

    pub(super) fn max_retained_tasks(&self) -> i64 {
        if self.paid != 0 {
            MAX_RETAINED_TASKS_PAID
        } else {
            MAX_RETAINED_TASKS
        }
    }

    pub(super) fn can_attempt(&self, now: i64) -> bool {
        now < self.expires_ms && self.attempts_used < self.attempt_budget
    }
}

pub(super) fn no_project() -> ApiError {
    ApiError::not_found("project_not_found", "create the project first")
}

impl<D: Db> Ledger<'_, D> {
    pub(super) fn project_row(&self) -> Result<Option<ProjectRow>, DbError> {
        Ok(self
            .db
            .query(
                "SELECT id, created_ms, expires_ms, attempt_budget, attempts_used, signing_secret, \
                 EXISTS (SELECT 1 FROM payments) AS paid, inbox_last_seq, inbox_acked_seq, \
                 inbox_dropped, (SELECT COUNT(*) FROM inbox WHERE seq > p.inbox_acked_seq) \
                 AS inbox_unread FROM project p LIMIT 1",
                vec![],
            )?
            .pop())
    }

    pub fn project(&self) -> Result<ProjectView, ApiError> {
        let mut view = self.project_row()?.ok_or_else(no_project)?.view();
        view.wake = self.wake_view()?;
        Ok(view)
    }

    /// Creates the project with a fresh trial, or returns the existing one unchanged.
    /// `signing_secret` is only used on creation. The view carries the project's signing secret
    /// either way: calling again is how to fetch it.
    pub fn create_project(
        &self,
        project_id: &str,
        signing_secret: &str,
        now: i64,
    ) -> Result<(ProjectView, bool), ApiError> {
        let existing = self.project_row()?.map(|p| p.signing_secret);
        let created = existing.is_none();
        if created {
            self.db.exec(
                "INSERT INTO project (id, created_ms, expires_ms, attempt_budget, attempts_used, \
                 signing_secret) VALUES (?, ?, ?, ?, 0, ?)",
                vec![
                    project_id.into(),
                    now.into(),
                    (now + TRIAL_DURATION_MS).into(),
                    TRIAL_ATTEMPTS.into(),
                    signing_secret.into(),
                ],
            )?;
        }
        let mut view = self.project()?;
        view.signing_secret = existing.or_else(|| Some(signing_secret.into()));
        Ok((view, created))
    }

    /// Adds `attempts` bought with payment `payment_id`, and makes the whole balance valid for
    /// [`CREDIT_VALIDITY_MS`] from now. Attempts left from an earlier purchase or the trial carry
    /// over unless they have already expired. Crons stopped by expiry start again. Returns the
    /// project and whether the payment was new; a payment already applied changes nothing.
    pub fn add_credit(
        &self,
        payment_id: &str,
        attempts: i64,
        now: i64,
    ) -> Result<(ProjectView, bool), ApiError> {
        if payment_id.is_empty()
            || payment_id.len() > 255
            || !payment_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(
                ApiError::bad_request("payment_id is not a valid payment id")
                    .in_field("payment_id"),
            );
        }
        if !(1..=MAX_CREDIT_ATTEMPTS).contains(&attempts) {
            return Err(
                ApiError::bad_request(format!("attempts must be 1-{MAX_CREDIT_ATTEMPTS}"))
                    .in_field("attempts"),
            );
        }
        let project = self.project_row()?.ok_or_else(no_project)?;
        let applied: Vec<Counts> = self.db.query(
            "SELECT COUNT(*) AS total, 0 AS waiting FROM payments WHERE id = ?",
            vec![payment_id.into()],
        )?;
        if applied.first().map_or(0, |c| c.total) > 0 {
            return Ok((self.project()?, false));
        }
        let left = if now < project.expires_ms {
            (project.attempt_budget - project.attempts_used).max(0)
        } else {
            0
        };
        // Credit before recording the payment: if recording fails, Stripe's retry credits again,
        // which costs AgentLater a little; the other order could leave a paid pack never credited.
        self.db.exec(
            "UPDATE project SET attempt_budget = ?, expires_ms = ?",
            vec![
                (project.attempts_used + left + attempts).into(),
                project.expires_ms.max(now + CREDIT_VALIDITY_MS).into(),
            ],
        )?;
        self.db.exec(
            "INSERT INTO payments (id, attempts, created_ms) VALUES (?, ?, ?)",
            vec![payment_id.into(), attempts.into(), now.into()],
        )?;
        self.restart_stopped_crons(now)?;
        Ok((self.project()?, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::Memory;
    use crate::ledger::testing::*;

    #[test]
    fn creates_a_project_once() {
        let db = Memory::new();
        let l = Ledger::open(&db).unwrap();
        assert_eq!(l.project().unwrap_err().code, "project_not_found");

        let (p, created) = l.create_project(P, "s1", T0).unwrap();
        assert!(created);
        assert_eq!(p.attempts_remaining, i64::from(TRIAL_ATTEMPTS));
        assert_eq!(p.expires_ms, T0 + TRIAL_DURATION_MS);

        let (again, created) = l.create_project(P, "s2", T0 + 5).unwrap();
        assert!(!created);
        assert_eq!(
            again, p,
            "a second create must not refill or rekey the trial"
        );
        assert_eq!(p.signing_secret.as_deref(), Some("s1"));
    }

    #[test]
    fn only_create_project_returns_the_signing_secret() {
        let db = Memory::new();
        let l = setup(&db);
        assert_eq!(l.project().unwrap().signing_secret, None);
        let (credited, _) = l.add_credit("cs_1", 5_000, T0).unwrap();
        assert_eq!(credited.signing_secret, None);
        let json = serde_json::to_value(l.project().unwrap()).unwrap();
        assert!(json.get("signing_secret").is_none(), "the key is left out");
    }

    #[test]
    fn credit_adds_attempts_once_and_extends_expiry() {
        let db = Memory::new();
        let l = setup(&db);
        l.schedule(req("https://a.example.com/"), "t1", T0).unwrap();
        db.0.execute("UPDATE project SET attempts_used = 30", [])
            .unwrap();

        let (p, added) = l.add_credit("cs_test_1", 50_000, T0 + 10).unwrap();
        assert!(added);
        assert!(p.paid);
        assert_eq!(
            p.attempts_remaining,
            i64::from(TRIAL_ATTEMPTS) - 30 + 50_000,
            "unused trial attempts carry over"
        );
        assert_eq!(p.expires_ms, T0 + 10 + CREDIT_VALIDITY_MS);

        let (again, added) = l.add_credit("cs_test_1", 50_000, T0 + 20).unwrap();
        assert!(!added);
        assert_eq!(again, p, "a repeated payment must not add twice");

        let (two, _) = l.add_credit("cs_test_2", 50_000, T0 + 30).unwrap();
        assert_eq!(
            two.attempts_remaining,
            i64::from(TRIAL_ATTEMPTS) - 30 + 100_000
        );
        assert_eq!(two.expires_ms, T0 + 30 + CREDIT_VALIDITY_MS);
    }

    #[test]
    fn credit_after_expiry_drops_expired_attempts_and_reopens_the_project() {
        let db = Memory::new();
        let l = setup(&db);
        let end = T0 + TRIAL_DURATION_MS;
        assert_eq!(
            l.schedule(req("https://a.example.com/"), "t1", end)
                .unwrap_err()
                .code,
            "trial_expired"
        );
        let (p, _) = l.add_credit("cs_1", 50_000, end).unwrap();
        assert_eq!(
            p.attempts_remaining, 50_000,
            "expired trial attempts are not revived"
        );
        l.schedule(req("https://a.example.com/"), "t1", end)
            .unwrap();
    }

    #[test]
    fn credit_restarts_crons_stopped_by_expiry() {
        let db = Memory::new();
        let l = setup(&db);
        l.put_cron(daily("backup"), T0).unwrap();
        let end = T0 + TRIAL_DURATION_MS;
        l.fire_crons(end).unwrap();
        assert_eq!(l.cron("backup").unwrap().next_run_ms, None);

        l.add_credit("cs_1", 50_000, end).unwrap();
        assert!(l.cron("backup").unwrap().next_run_ms.unwrap() > end);
        assert!(l.next_wake().unwrap().is_some());
    }

    #[test]
    fn credit_rejects_bad_arguments() {
        let db = Memory::new();
        let l = Ledger::open(&db).unwrap();
        assert_eq!(
            l.add_credit("cs_1", 1, T0).unwrap_err().code,
            "project_not_found"
        );
        let l = setup(&db);
        for (id, attempts, field) in [
            ("", 1, "payment_id"),
            ("cs 1", 1, "payment_id"),
            ("cs_1", 0, "attempts"),
            ("cs_1", MAX_CREDIT_ATTEMPTS + 1, "attempts"),
        ] {
            assert_eq!(
                l.add_credit(id, attempts, T0).unwrap_err().field,
                Some(field)
            );
        }
        assert!(!l.project().unwrap().paid);
    }

    #[test]
    fn a_paid_project_holds_more_tasks() {
        let db = Memory::new();
        let l = setup(&db);
        l.add_credit("cs_1", 5_000, T0).unwrap();
        for i in 0..=MAX_RETAINED_TASKS {
            l.schedule(req("https://a.example.com/"), &format!("t{i}"), T0)
                .unwrap();
        }
    }
}
