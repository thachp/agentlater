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
    /// When the signing secret that a rotation replaced stops signing; null outside an overlap.
    #[serde(default)]
    pub signing_secret_previous_until_ms: Option<i64>,
    /// Inbox events not yet acked; `read_inbox` returns them.
    #[serde(default)]
    pub inbox_unread: i64,
    /// Where AgentLater sends a wake when the inbox gets something; see `set_wake`.
    #[serde(default)]
    pub wake: Option<WakeView>,
}

/// What `rotate_signing_secret` answers: the only other place a signing secret is shown.
#[derive(Debug, Serialize)]
pub struct Rotated {
    pub signing_secret: String,
    /// When the secret this one replaced stops signing.
    pub previous_valid_until_ms: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct ProjectRow {
    pub(super) id: String,
    created_ms: i64,
    pub(super) expires_ms: i64,
    pub(super) attempt_budget: i64,
    pub(super) attempts_used: i64,
    signing_secret: String,
    signing_secret_previous: Option<String>,
    signing_secret_previous_until_ms: Option<i64>,
    pub(super) paid: i64,
    pub(super) inbox_last_seq: i64,
    pub(super) inbox_acked_seq: i64,
    pub(super) inbox_dropped: i64,
    pub(super) inbox_unread: i64,
}

impl ProjectRow {
    fn view(self, now: i64) -> ProjectView {
        ProjectView {
            signing_secret_previous_until_ms: self.previous_until(now),
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

    /// When the previous signing secret stops, while it is still signing at `now`.
    fn previous_until(&self, now: i64) -> Option<i64> {
        self.signing_secret_previous_until_ms
            .filter(|until| now < *until)
    }

    /// The secrets that sign a delivery made at `now`, newest first.
    pub(super) fn signing_secrets(self, now: i64) -> Vec<String> {
        let previous = self.previous_until(now).and(self.signing_secret_previous);
        std::iter::once(self.signing_secret)
            .chain(previous)
            .collect()
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
                 signing_secret_previous, signing_secret_previous_until_ms, \
                 EXISTS (SELECT 1 FROM payments) AS paid, inbox_last_seq, inbox_acked_seq, \
                 inbox_dropped, (SELECT COUNT(*) FROM inbox WHERE seq > p.inbox_acked_seq) \
                 AS inbox_unread FROM project p LIMIT 1",
                vec![],
            )?
            .pop())
    }

    pub fn project(&self, now: i64) -> Result<ProjectView, ApiError> {
        let mut view = self.project_row()?.ok_or_else(no_project)?.view(now);
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
        let mut view = self.project(now)?;
        view.signing_secret = existing.or_else(|| Some(signing_secret.into()));
        Ok((view, created))
    }

    /// Makes `new_secret` the project's signing secret. The one it replaces keeps signing beside
    /// it for `overlap` (a duration like a wake's debounce; `"0s"` stops it at once), so a
    /// receiver holding either verifies. A secret still in an earlier overlap stops now: at most
    /// two sign.
    pub fn rotate_signing_secret(
        &self,
        new_secret: &str,
        overlap: Option<&str>,
        now: i64,
    ) -> Result<Rotated, ApiError> {
        let project = self.project_row()?.ok_or_else(no_project)?;
        let overlap_ms = parse_delay(overlap.unwrap_or(DEFAULT_ROTATION_OVERLAP))
            .filter(|ms| *ms <= MAX_ROTATION_OVERLAP_MS)
            .ok_or_else(|| {
                ApiError::bad_request(
                    "overlap must be a whole number followed by s, m, h or d, from \"0s\" to \"7d\"",
                )
                .in_field("overlap")
            })?;
        let until = (overlap_ms > 0).then_some(now + overlap_ms);
        self.db.exec(
            "UPDATE project SET signing_secret = ?, signing_secret_previous = ?, \
             signing_secret_previous_until_ms = ?",
            vec![
                new_secret.into(),
                until.and(Some(project.signing_secret)).into(),
                until.into(),
            ],
        )?;
        Ok(Rotated {
            signing_secret: new_secret.into(),
            previous_valid_until_ms: until,
        })
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
        let applied = self.count(
            "SELECT COUNT(*) AS total FROM payments WHERE id = ?",
            vec![payment_id.into()],
        )?;
        if applied > 0 {
            return Ok((self.project(now)?, false));
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
        Ok((self.project(now)?, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::Memory;
    use crate::ledger::testing::*;
    use runlater_core::limits::HOUR_MS;

    #[test]
    fn creates_a_project_once() {
        let db = Memory::new();
        let l = Ledger::open(&db).unwrap();
        assert_eq!(l.project(T0).unwrap_err().code, "project_not_found");

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
        assert_eq!(l.project(T0).unwrap().signing_secret, None);
        let (credited, _) = l.add_credit("cs_1", 5_000, T0).unwrap();
        assert_eq!(credited.signing_secret, None);
        let json = serde_json::to_value(l.project(T0).unwrap()).unwrap();
        assert!(json.get("signing_secret").is_none(), "the key is left out");
    }

    #[test]
    fn rotating_returns_a_new_secret_that_create_project_then_returns() {
        let db = Memory::new();
        let l = setup(&db);
        let rotated = l.rotate_signing_secret("s2", None, T0).unwrap();
        assert_eq!(rotated.signing_secret, "s2");
        let (again, created) = l.create_project(P, "s3", T0 + 1).unwrap();
        assert!(!created);
        assert_eq!(again.signing_secret.as_deref(), Some("s2"));
    }

    /// The secrets that sign a delivery made at `now`, newest first.
    fn signing(l: &Ledger<'_, Memory>, id: &str, now: i64) -> Vec<String> {
        l.schedule(req("https://a.example.com/"), id, now).unwrap();
        drive(l, id, http(200), None, now).0.signing_secrets
    }

    #[test]
    fn both_secrets_sign_during_the_overlap_and_only_the_new_one_after() {
        let db = Memory::new();
        let l = setup(&db);
        let rotated = l.rotate_signing_secret("s2", Some("1h"), T0).unwrap();
        assert_eq!(rotated.previous_valid_until_ms, Some(T0 + HOUR_MS));
        assert_eq!(signing(&l, "t1", T0 + HOUR_MS - 1), ["s2", "secret"]);
        assert_eq!(signing(&l, "t2", T0 + HOUR_MS), ["s2"]);
    }

    #[test]
    fn the_overlap_defaults_to_24_hours_and_zero_stops_the_old_secrets_at_once() {
        let db = Memory::new();
        let l = setup(&db);
        let rotated = l.rotate_signing_secret("s2", None, T0).unwrap();
        assert_eq!(rotated.previous_valid_until_ms, Some(T0 + DAY));
        let at_once = l.rotate_signing_secret("s3", Some("0s"), T0 + 1).unwrap();
        assert_eq!(at_once.previous_valid_until_ms, None);
        assert_eq!(signing(&l, "t1", T0 + 1), ["s3"]);
    }

    #[test]
    fn an_overlap_is_at_most_7_days_and_an_invalid_one_names_the_field() {
        let db = Memory::new();
        let l = setup(&db);
        for bad in ["soon", "0", "", "-1h", "8d"] {
            let e = l.rotate_signing_secret("s2", Some(bad), T0).unwrap_err();
            assert_eq!((e.status, e.field), (400, Some("overlap")), "{bad:?}");
        }
        assert_eq!(signing(&l, "t1", T0), ["secret"], "nothing was rotated");
        let longest = l.rotate_signing_secret("s2", Some("7d"), T0).unwrap();
        assert_eq!(longest.previous_valid_until_ms, Some(T0 + 7 * DAY));
    }

    #[test]
    fn rotating_again_during_an_overlap_keeps_the_two_newest_secrets() {
        let db = Memory::new();
        let l = setup(&db);
        l.rotate_signing_secret("s2", Some("1h"), T0).unwrap();
        l.rotate_signing_secret("s3", Some("1h"), T0 + 1).unwrap();
        assert_eq!(signing(&l, "t1", T0 + 2), ["s3", "s2"]);
    }

    #[test]
    fn the_project_shows_when_the_previous_secret_stops_and_no_secret() {
        let db = Memory::new();
        let l = setup(&db);
        assert_eq!(
            l.project(T0).unwrap().signing_secret_previous_until_ms,
            None
        );
        l.rotate_signing_secret("s2", Some("1h"), T0).unwrap();
        let during = serde_json::to_value(l.project(T0 + HOUR_MS - 1).unwrap()).unwrap();
        assert_eq!(during["signing_secret_previous_until_ms"], T0 + HOUR_MS);
        let shown = during.to_string();
        assert!(!shown.contains("s2") && !shown.contains("\"secret\""));
        let after = serde_json::to_value(l.project(T0 + HOUR_MS).unwrap()).unwrap();
        assert!(after["signing_secret_previous_until_ms"].is_null());
        assert!(after.get("signing_secret").is_none());
    }

    #[test]
    fn rotating_needs_a_project_and_changes_nothing_else() {
        let db = Memory::new();
        let l = Ledger::open(&db).unwrap();
        assert_eq!(
            l.rotate_signing_secret("s2", None, T0).unwrap_err().code,
            "project_not_found"
        );
        let l = setup(&db);
        l.schedule(req("https://a.example.com/"), "t1", T0).unwrap();
        let before = l.project(T0).unwrap();
        let tasks = l.list(None, None, None, None).unwrap();
        l.rotate_signing_secret("s2", Some("0s"), T0).unwrap();
        assert_eq!(l.project(T0).unwrap(), before);
        assert_eq!(l.list(None, None, None, None).unwrap(), tasks);
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
        assert!(!l.project(T0).unwrap().paid);
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
