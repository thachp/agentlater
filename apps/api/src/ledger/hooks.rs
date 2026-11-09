//! Inbound hooks: each delivery to a hook's URL becomes a `hook.received` inbox event.
//!
//! The ledger stores a hook by the SHA-256 of its token, so not even a copy of the database
//! gives the URL back. It checks signatures, applies the rate limits, drops retries the sender
//! marked with a delivery id, and writes the event.

use super::*;
use base64::Engine;
use runlater_core::limits::{
    DAY_MS, HOOK_BODY_KEPT_BYTES, HOOK_BURST, HOOK_DEDUPE_MS, HOOK_EVENTS_PER_DAY,
    HOOK_EVENTS_PER_DAY_PAID, HOOK_INTERVAL_MS, HOOK_UNVERIFIED_EVENTS_PER_DAY, HOUR_MS, MAX_HOOKS,
    MAX_HOOK_REQUEST_BYTES,
};
use runlater_core::{
    accepts, check_keep, cut, delivery_id, kept_headers, AcceptRule, HookPatch, HookSpec,
    VerifySpec,
};

/// A hook as callers see it: never its URL or secret.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct HookView {
    pub name: String,
    pub verify: Option<VerifySpec>,
    pub keep_headers: Vec<String>,
    /// The rules a request must match to be kept; none means every request is.
    pub accept: Option<Vec<AcceptRule>>,
    pub created_ms: i64,
    pub last_received_ms: Option<i64>,
    /// Requests written to the inbox.
    pub received: i64,
    /// Requests refused, by reason: signature_missing, signature_invalid, signature_stale,
    /// rate_limited, daily_limit, inbox_full or project_expired.
    pub refused: BTreeMap<String, i64>,
    /// Requests that matched no `accept` rule: answered 202 and dropped.
    pub filtered: i64,
}

/// `create_hook`'s answer: the hook, and the path of its URL, shown this once.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct CreatedHook {
    #[serde(flatten)]
    pub hook: HookView,
    pub url_path: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct HookList {
    pub hooks: Vec<HookView>,
}

/// A hook as the receive path needs it.
#[derive(Deserialize)]
pub struct HookRow {
    pub name: String,
    verify: Option<String>,
    /// The verify secret, sealed.
    pub sealed_secret: Option<String>,
    keep_headers: String,
    created_ms: i64,
    last_received_ms: Option<i64>,
    received: i64,
    refused: String,
    /// When the rate limit's next request is due on average (GCRA).
    rate_tat_ms: i64,
    /// Events this hook wrote on UTC day `day`; capped when the hook is unverified.
    day: i64,
    day_count: i64,
    accept: Option<String>,
    filtered: i64,
}

const HOOK_COLUMNS: &str = "name, verify, sealed_secret, keep_headers, created_ms, \
    last_received_ms, received, refused, rate_tat_ms, day, day_count, accept, filtered";

impl HookRow {
    pub fn verify(&self) -> Result<Option<VerifySpec>, DbError> {
        self.verify
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|e| DbError(e.to_string()))
    }

    pub fn accept(&self) -> Result<Option<Vec<AcceptRule>>, DbError> {
        self.accept
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|e| DbError(e.to_string()))
    }

    pub fn keep_headers(&self) -> Result<Vec<String>, DbError> {
        serde_json::from_str(&self.keep_headers).map_err(|e| DbError(e.to_string()))
    }

    fn view(&self) -> Result<HookView, DbError> {
        Ok(HookView {
            name: self.name.clone(),
            verify: self.verify()?,
            keep_headers: self.keep_headers()?,
            accept: self.accept()?,
            created_ms: self.created_ms,
            last_received_ms: self.last_received_ms,
            received: self.received,
            refused: serde_json::from_str(&self.refused).map_err(|e| DbError(e.to_string()))?,
            filtered: self.filtered,
        })
    }
}

/// What the API kept of one request to a hook.
#[derive(Debug)]
pub struct Received<'r> {
    /// Whether a signature was checked and matched.
    pub verified: bool,
    pub delivery_id: Option<String>,
    pub headers: BTreeMap<String, String>,
    /// At most `HOOK_BODY_KEPT_BYTES`, as text.
    pub body: &'r str,
    pub body_bytes: usize,
    /// Whether `body` is less than the whole request. Not `body.len() < body_bytes`: bytes
    /// that aren't UTF-8 become three bytes each as text.
    pub truncated: bool,
}

/// How a request to a hook was answered.
#[derive(Debug, PartialEq, Eq)]
pub enum HookOutcome {
    /// Written to the inbox as this event.
    Accepted(i64),
    /// Matched no `accept` rule: answered as if accepted, and dropped.
    Filtered,
    /// A delivery id seen in the last day: a retry, already in the inbox.
    Duplicate,
    /// Not written: the status to answer, why, and when to try again.
    Refused {
        status: u16,
        reason: &'static str,
        retry_after_ms: Option<i64>,
    },
}

/// A request a sender made to a hook, as the gateway forwards it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookDelivery {
    pub token: String,
    pub headers: Vec<(String, String)>,
    /// The raw body, base64: the signature is over its exact bytes.
    pub body: String,
}

/// What a hook's sender is told: a status, and why it was refused. Never anything it sent.
#[derive(Debug, PartialEq, Eq)]
pub struct HookReply {
    pub status: u16,
    pub reason: Option<&'static str>,
    pub retry_after_ms: Option<i64>,
}

impl HookReply {
    fn ok(status: u16) -> Self {
        HookReply {
            status,
            reason: None,
            retry_after_ms: None,
        }
    }

    fn refused(status: u16, reason: &'static str) -> Self {
        HookReply {
            status,
            reason: Some(reason),
            retry_after_ms: None,
        }
    }
}

/// The SHA-256 a hook's token is stored and looked up by.
pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

impl<D: Db> Ledger<'_, D> {
    fn hook_row(&self, name: &str) -> Result<Option<HookRow>, DbError> {
        Ok(self
            .db
            .query(
                &format!("SELECT {HOOK_COLUMNS} FROM hooks WHERE name = ?"),
                vec![name.into()],
            )?
            .pop())
    }

    /// Creates a hook, or replaces one of the same name with a new URL: the old URL stops
    /// working at once, and the counts start again. `token` is random from the API.
    pub fn put_hook(
        &self,
        spec: &HookSpec,
        token: &str,
        now: i64,
    ) -> Result<(CreatedHook, bool), ApiError> {
        let project = self.project_row()?.ok_or_else(no_project)?;
        if now >= project.expires_ms {
            return Err(ApiError::new(
                403,
                "trial_expired",
                "this project's attempts have expired",
            ));
        }
        let existing = self.hook_row(&spec.name)?;
        if existing.is_none() {
            let count: Vec<Counts> = self
                .db
                .query("SELECT COUNT(*) AS total, 0 AS waiting FROM hooks", vec![])?;
            if count.first().map_or(0, |c| c.total) >= MAX_HOOKS {
                return Err(ApiError::new(
                    429,
                    "hook_limit",
                    format!("this project already has {MAX_HOOKS} hooks; delete one first"),
                ));
            }
        }
        let sealed_secret = match &spec.verify {
            Some((_, secret)) => Some(self.seal_hook_secret(&project.id, &spec.name, secret)?),
            None => None,
        };
        let verify = spec
            .verify
            .as_ref()
            .map(|(v, _)| serde_json::to_string(v))
            .transpose()
            .map_err(|e| DbError(e.to_string()))?;
        let keep = serde_json::to_string(&spec.keep_headers).map_err(|e| DbError(e.to_string()))?;
        let accept = spec
            .accept
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| DbError(e.to_string()))?;
        self.db.exec(
            "DELETE FROM hooks WHERE name = ?",
            vec![spec.name.as_str().into()],
        )?;
        self.db.exec(
            "DELETE FROM hook_deliveries WHERE hook = ?",
            vec![spec.name.as_str().into()],
        )?;
        self.db.exec(
            "INSERT INTO hooks (name, token_hash, verify, sealed_secret, keep_headers, accept, \
             created_ms, received, refused, rate_tat_ms) VALUES (?, ?, ?, ?, ?, ?, ?, 0, '{}', 0)",
            vec![
                spec.name.as_str().into(),
                token_hash(token).into(),
                verify.into(),
                sealed_secret.into(),
                keep.into(),
                accept.into(),
                now.into(),
            ],
        )?;
        let row = self
            .hook_row(&spec.name)?
            .ok_or_else(|| DbError("hook vanished".into()))?;
        let created = CreatedHook {
            hook: row.view()?,
            url_path: format!("/h/{}/{token}", project.id),
        };
        Ok((created, existing.is_none()))
    }

    pub fn hooks(&self) -> Result<HookList, ApiError> {
        self.project_row()?.ok_or_else(no_project)?;
        let rows: Vec<HookRow> = self.db.query(
            &format!("SELECT {HOOK_COLUMNS} FROM hooks ORDER BY name"),
            vec![],
        )?;
        let hooks = rows.iter().map(HookRow::view).collect::<Result<_, _>>()?;
        Ok(HookList { hooks })
    }

    /// Changes a hook's `verify`, `accept` or `keep_headers`, keeping its URL, counts and the
    /// delivery ids it has seen. A new `verify`'s secret is sealed here.
    pub fn update_hook(&self, name: &str, patch: HookPatch) -> Result<HookView, ApiError> {
        let project = self.project_row()?.ok_or_else(no_project)?;
        let row = self.hook_row(name)?.ok_or_else(|| {
            ApiError::not_found("hook_not_found", "no hook with that name in this project")
        })?;
        let (verify, sealed) = match patch.verify {
            None => (row.verify()?, row.sealed_secret.clone()),
            Some(None) => (None, None),
            Some(Some((spec, secret))) => (
                Some(spec),
                Some(self.seal_hook_secret(&project.id, name, &secret)?),
            ),
        };
        // The headers kept must still pass with the verify that now applies.
        let keep = check_keep(
            verify.as_ref(),
            patch.keep_headers.unwrap_or(row.keep_headers()?),
        )
        .map_err(|e| ApiError {
            field: Some(e.field()),
            ..ApiError::bad_request(e.to_string())
        })?;
        let accept = match patch.accept {
            None => row.accept()?,
            Some(a) => a,
        };
        let to_json = |e: serde_json::Error| DbError(e.to_string());
        self.db.exec(
            "UPDATE hooks SET verify = ?, sealed_secret = ?, keep_headers = ?, accept = ? \
             WHERE name = ?",
            vec![
                verify
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(to_json)?
                    .into(),
                sealed.into(),
                serde_json::to_string(&keep).map_err(to_json)?.into(),
                accept
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(to_json)?
                    .into(),
                name.into(),
            ],
        )?;
        Ok(self
            .hook_row(name)?
            .ok_or_else(|| DbError("hook vanished".into()))?
            .view()?)
    }

    /// Removes a hook; its URL answers 404 from now on. Events it wrote stay in the inbox.
    pub fn delete_hook(&self, name: &str) -> Result<HookView, ApiError> {
        let row = self.hook_row(name)?.ok_or_else(|| {
            ApiError::not_found("hook_not_found", "no hook with that name in this project")
        })?;
        self.db
            .exec("DELETE FROM hooks WHERE name = ?", vec![name.into()])?;
        self.db.exec(
            "DELETE FROM hook_deliveries WHERE hook = ?",
            vec![name.into()],
        )?;
        Ok(row.view()?)
    }

    /// The hook a request's token belongs to, if any.
    pub fn hook_for_token(&self, token: &str) -> Result<Option<HookRow>, DbError> {
        Ok(self
            .db
            .query(
                &format!("SELECT {HOOK_COLUMNS} FROM hooks WHERE token_hash = ?"),
                vec![token_hash(token).into()],
            )?
            .pop())
    }

    /// Counts a request refused before it reached [`Self::receive_hook`] (a bad signature).
    pub fn hook_refused(&self, hook: &str, reason: &str) -> Result<(), DbError> {
        let Some(row) = self.hook_row(hook)? else {
            return Ok(());
        };
        let mut refused: BTreeMap<String, i64> =
            serde_json::from_str(&row.refused).map_err(|e| DbError(e.to_string()))?;
        *refused.entry(reason.into()).or_default() += 1;
        let refused = serde_json::to_string(&refused).map_err(|e| DbError(e.to_string()))?;
        self.db.exec(
            "UPDATE hooks SET refused = ? WHERE name = ?",
            vec![refused.into(), hook.into()],
        )
    }

    /// Takes a request a sender made to a hook: finds the hook by its token, checks the
    /// signature if the hook has one, and writes what passes. Without the key that
    /// opens the hook's secret nothing can be checked, so the sender is told to try again later
    /// (503).
    pub fn take_hook_request(&self, d: HookDelivery, now: i64) -> Result<HookReply, DbError> {
        let Some(hook) = self.hook_for_token(&d.token)? else {
            return Ok(HookReply::refused(404, "hook_not_found"));
        };
        let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(&d.body) else {
            return Ok(HookReply::refused(400, "invalid_request"));
        };
        if raw.len() > MAX_HOOK_REQUEST_BYTES {
            return Ok(HookReply::refused(413, "too_large"));
        }
        let headers: Vec<(String, String)> = d
            .headers
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect();
        let check = hook.verify()?;
        if let Some(spec) = &check {
            let secret = match &hook.sealed_secret {
                Some(sealed) => self.open_hook_secret(&hook.name, sealed)?,
                None => None,
            };
            let Some(secret) = secret else {
                return Ok(HookReply::refused(503, "secrets_unavailable"));
            };
            if let Err(why) = runlater_core::verify(spec, secret.as_bytes(), &headers, &raw, now) {
                self.hook_refused(&hook.name, why.as_str())?;
                return Ok(HookReply::refused(401, why.as_str()));
            }
        }
        let keep = hook.keep_headers()?;
        let rules = hook.accept()?;
        let matches = || rules.as_ref().is_none_or(|r| accepts(r, &headers, &raw));
        let text = String::from_utf8_lossy(&raw);
        let body = cut(&text, HOOK_BODY_KEPT_BYTES);
        let received = Received {
            verified: check.is_some(),
            delivery_id: delivery_id(check.as_ref(), &headers, &raw),
            headers: kept_headers(check.as_ref(), &keep, &headers),
            body,
            body_bytes: raw.len(),
            truncated: body.len() < text.len(),
        };
        Ok(match self.receive_hook(&hook, received, matches, now)? {
            HookOutcome::Accepted(_) | HookOutcome::Filtered => HookReply::ok(202),
            HookOutcome::Duplicate => HookReply::ok(200),
            HookOutcome::Refused {
                status,
                reason,
                retry_after_ms,
            } => HookReply {
                status,
                reason: Some(reason),
                retry_after_ms,
            },
        })
    }

    /// Writes a request the API accepted (signature checked, if the hook has one) as an inbox
    /// event, unless the project is over its limits or the sender is retrying.
    ///
    /// `matches` says whether the request passes the hook's `accept` rules. It is only asked
    /// once the request is within the hook's rate limit, so matching (which may parse the body)
    /// costs nothing for a flood that is turned away anyway.
    pub fn receive_hook(
        &self,
        hook: &HookRow,
        r: Received<'_>,
        matches: impl FnOnce() -> bool,
        now: i64,
    ) -> Result<HookOutcome, DbError> {
        let project = self
            .project_row()?
            .ok_or_else(|| DbError("no project for the hook".into()))?;
        let refuse = |status, reason, retry_after_ms| -> Result<HookOutcome, DbError> {
            self.hook_refused(&hook.name, reason)?;
            Ok(HookOutcome::Refused {
                status,
                reason,
                retry_after_ms,
            })
        };
        if now >= project.expires_ms {
            return refuse(410, "project_expired", None);
        }
        // GCRA: one request per interval on average, `HOOK_BURST` at once.
        let tat = hook.rate_tat_ms.max(now);
        let allowance = (HOOK_BURST - 1) * HOOK_INTERVAL_MS;
        if tat - now > allowance {
            return refuse(429, "rate_limited", Some(tat - now - allowance));
        }
        // Not what the agent asked for: answered 202 like the rest, so the sender doesn't retry,
        // but nothing is written or woken, and no day is used. Nor is the rate limit: a busy
        // sender's noise must not crowd out the one request that matters. (The gateway's limits
        // per IP and per project still bound it.)
        if !matches() {
            self.db.exec(
                "UPDATE hooks SET filtered = filtered + 1 WHERE name = ?",
                vec![hook.name.as_str().into()],
            )?;
            return Ok(HookOutcome::Filtered);
        }
        self.db.exec(
            "UPDATE hooks SET rate_tat_ms = ? WHERE name = ?",
            vec![(tat + HOOK_INTERVAL_MS).into(), hook.name.as_str().into()],
        )?;

        if let Some(id) = &r.delivery_id {
            self.db.exec(
                "DELETE FROM hook_deliveries WHERE hook = ? AND expires_ms <= ?",
                vec![hook.name.as_str().into(), now.into()],
            )?;
            let seen: Vec<Counts> = self.db.query(
                "SELECT COUNT(*) AS total, 0 AS waiting FROM hook_deliveries \
                 WHERE hook = ? AND delivery_id = ?",
                vec![hook.name.as_str().into(), id.as_str().into()],
            )?;
            if seen.first().map_or(0, |c| c.total) > 0 {
                return Ok(HookOutcome::Duplicate);
            }
        }

        let day = now.div_euclid(DAY_MS);
        let cap = if project.paid != 0 {
            HOOK_EVENTS_PER_DAY_PAID
        } else {
            HOOK_EVENTS_PER_DAY
        };
        let today: Vec<DayRow> = self
            .db
            .query("SELECT hook_day, hook_day_count FROM project", vec![])?;
        let count = today
            .first()
            .filter(|t| t.hook_day == day)
            .map_or(0, |t| t.hook_day_count);
        let tomorrow = Some((day + 1) * DAY_MS - now);
        if count >= cap {
            return refuse(429, "daily_limit", tomorrow);
        }
        // Anyone with an unverified hook's URL can send to it, so it has a small day of its own.
        let hook_count = if hook.day == day { hook.day_count } else { 0 };
        if hook.verify.is_none() && hook_count >= HOOK_UNVERIFIED_EVENTS_PER_DAY {
            return refuse(429, "daily_limit", tomorrow);
        }
        if !self.room_for_hook_event()? {
            return refuse(429, "inbox_full", Some(HOUR_MS));
        }
        self.db.exec(
            "UPDATE project SET hook_day = ?, hook_day_count = ?",
            vec![day.into(), (count + 1).into()],
        )?;
        self.db.exec(
            "UPDATE hooks SET day = ?, day_count = ? WHERE name = ?",
            vec![
                day.into(),
                (hook_count + 1).into(),
                hook.name.as_str().into(),
            ],
        )?;

        if let Some(id) = &r.delivery_id {
            self.db.exec(
                "INSERT INTO hook_deliveries (hook, delivery_id, expires_ms) VALUES (?, ?, ?)",
                vec![
                    hook.name.as_str().into(),
                    id.as_str().into(),
                    (now + HOOK_DEDUPE_MS).into(),
                ],
            )?;
        }
        let event = serde_json::json!({
            "event": "hook.received",
            "hook": hook.name,
            "verified": r.verified,
            "delivery_id": r.delivery_id,
            "request": {
                "method": "POST",
                "headers": r.headers,
                "body": r.body,
                "body_bytes": r.body_bytes,
                "truncated": r.truncated,
            },
        });
        self.db.exec(
            "UPDATE hooks SET received = received + 1, last_received_ms = ? WHERE name = ?",
            vec![now.into(), hook.name.as_str().into()],
        )?;
        let seq = self.post(now, |_| Ok(event))?;
        Ok(HookOutcome::Accepted(seq))
    }
}

#[derive(Deserialize)]
struct DayRow {
    hook_day: i64,
    hook_day_count: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::Memory;
    use runlater_core::limits::{CAPTURE_RETENTION_MS, MAX_INBOX_EVENTS, TRIAL_DURATION_MS};
    use runlater_core::{HookRequest, WakeRequest};

    #[test]
    fn token_hash_is_plain_sha256() {
        // Stored hashes are looked up by this, so it can never change.
        assert_eq!(
            token_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    const T0: i64 = 1_767_225_600_000;
    const P: &str = "proj_1";
    const TOKEN: &str = "hk_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn setup(db: &Memory) -> Ledger<'_, Memory> {
        let l = Ledger::open(db)
            .unwrap()
            .with_sealer(Some(secrets::test_sealer()));
        l.create_project(P, "secret", T0).unwrap();
        l
    }

    fn spec(v: serde_json::Value) -> HookSpec {
        serde_json::from_value::<HookRequest>(v)
            .unwrap()
            .validate()
            .unwrap()
    }

    fn hook(l: &Ledger<'_, Memory>, name: &str, token: &str) -> CreatedHook {
        l.put_hook(&spec(serde_json::json!({ "name": name })), token, T0)
            .unwrap()
            .0
    }

    fn received(delivery_id: Option<&str>) -> Received<'static> {
        Received {
            verified: delivery_id.is_some(),
            delivery_id: delivery_id.map(String::from),
            headers: BTreeMap::from([("content-type".into(), "application/json".into())]),
            body: r#"{"action":"completed"}"#,
            body_bytes: 22,
            truncated: false,
        }
    }

    fn send(
        l: &Ledger<'_, Memory>,
        token: &str,
        delivery_id: Option<&str>,
        now: i64,
    ) -> HookOutcome {
        let row = l.hook_for_token(token).unwrap().expect("a hook");
        l.receive_hook(&row, received(delivery_id), || true, now)
            .unwrap()
    }

    fn events(l: &Ledger<'_, Memory>) -> Vec<serde_json::Value> {
        l.read_inbox(None, Some(50), false).unwrap().events
    }

    #[test]
    fn creates_lists_and_deletes_hooks_without_their_url() {
        let db = Memory::new();
        let l = setup(&db);
        let s = spec(serde_json::json!({
            "name": "gh-ci",
            "verify": { "scheme": "github", "secret": "whsec_0123456789abcdef" },
            "keep_headers": ["X-Request-Id"],
        }));
        let (created, new) = l.put_hook(&s, TOKEN, T0).unwrap();
        assert!(new);
        assert_eq!(created.url_path, format!("/h/{P}/{TOKEN}"));
        assert_eq!(created.hook.keep_headers, ["x-request-id"]);
        assert_eq!(
            created.hook.verify.as_ref().unwrap().scheme.as_str(),
            "github"
        );

        let listed = serde_json::to_string(&l.hooks().unwrap()).unwrap();
        assert!(
            !listed.contains(TOKEN) && !listed.contains("v1:") && !listed.contains("whsec_"),
            "{listed}"
        );
        let stored: Vec<IdRow> = db
            .query("SELECT token_hash AS id FROM hooks", vec![])
            .unwrap();
        assert_eq!(stored[0].id, token_hash(TOKEN), "only the hash is stored");

        assert!(l.hook_for_token(TOKEN).unwrap().is_some());
        assert!(l.hook_for_token("hk_other").unwrap().is_none());
        l.delete_hook("gh-ci").unwrap();
        assert!(l.hook_for_token(TOKEN).unwrap().is_none());
        assert_eq!(l.delete_hook("gh-ci").unwrap_err().code, "hook_not_found");
    }

    #[test]
    fn replacing_a_hook_retires_its_old_url() {
        let db = Memory::new();
        let l = setup(&db);
        hook(&l, "h", TOKEN);
        send(&l, TOKEN, None, T0);
        let (again, new) = l
            .put_hook(&spec(serde_json::json!({ "name": "h" })), "hk_new", T0 + 1)
            .unwrap();
        assert!(!new);
        assert_eq!(again.hook.received, 0, "the counts start again");
        assert!(l.hook_for_token(TOKEN).unwrap().is_none());
        assert!(l.hook_for_token("hk_new").unwrap().is_some());
    }

    #[test]
    fn caps_the_hooks_a_project_holds() {
        let db = Memory::new();
        let l = setup(&db);
        for i in 0..MAX_HOOKS {
            hook(&l, &format!("h{i}"), &format!("hk_{i}"));
        }
        let e = l
            .put_hook(&spec(serde_json::json!({ "name": "one-more" })), "hk_x", T0)
            .unwrap_err();
        assert_eq!((e.status, e.code), (429, "hook_limit"));
        // Replacing one is not a new hook.
        l.put_hook(&spec(serde_json::json!({ "name": "h0" })), "hk_y", T0)
            .unwrap();
    }

    #[test]
    fn a_delivery_becomes_an_inbox_event_and_wakes_the_agent() {
        let db = Memory::new();
        let l = setup(&db);
        l.set_wake(
            serde_json::from_value::<WakeRequest>(serde_json::json!({
                "url": "https://agent.example.com/wake",
                "debounce": "1m",
            }))
            .unwrap(),
            T0,
        )
        .unwrap();
        hook(&l, "h", TOKEN);
        assert_eq!(send(&l, TOKEN, None, T0), HookOutcome::Accepted(1));
        let e = &events(&l)[0];
        assert_eq!(e["event"], "hook.received");
        assert_eq!(e["hook"], "h");
        assert_eq!(e["verified"], false);
        assert_eq!(e["request"]["body"], r#"{"action":"completed"}"#);
        assert_eq!(e["request"]["truncated"], false);
        assert_eq!(e["request"]["headers"]["content-type"], "application/json");
        assert_eq!(l.project().unwrap().inbox_unread, 1);
        let h = &l.hooks().unwrap().hooks[0];
        assert_eq!((h.received, h.last_received_ms), (1, Some(T0)));
        let wakes: Vec<IdRow> = db
            .query("SELECT id FROM tasks WHERE wake_call = 1", vec![])
            .unwrap();
        assert_eq!(
            wakes.len(),
            1,
            "a hook event wakes the agent like any other"
        );
    }

    #[test]
    fn a_retried_delivery_is_written_once() {
        let db = Memory::new();
        let l = setup(&db);
        hook(&l, "h", TOKEN);
        assert!(matches!(
            send(&l, TOKEN, Some("d-1"), T0),
            HookOutcome::Accepted(_)
        ));
        assert_eq!(
            send(&l, TOKEN, Some("d-1"), T0 + 1_000),
            HookOutcome::Duplicate
        );
        assert!(matches!(
            send(&l, TOKEN, Some("d-2"), T0 + 2_000),
            HookOutcome::Accepted(_)
        ));
        // A day later the id is forgotten.
        assert!(matches!(
            send(&l, TOKEN, Some("d-1"), T0 + HOOK_DEDUPE_MS + 1),
            HookOutcome::Accepted(_)
        ));
        assert_eq!(events(&l).len(), 3);
    }

    #[test]
    fn rate_limits_each_hook() {
        let db = Memory::new();
        let l = setup(&db);
        hook(&l, "h", TOKEN);
        hook(&l, "other", "hk_other");
        for _ in 0..HOOK_BURST {
            assert!(matches!(
                send(&l, TOKEN, None, T0),
                HookOutcome::Accepted(_)
            ));
        }
        assert_eq!(
            send(&l, TOKEN, None, T0),
            HookOutcome::Refused {
                status: 429,
                reason: "rate_limited",
                retry_after_ms: Some(HOOK_INTERVAL_MS)
            }
        );
        assert!(
            matches!(send(&l, "hk_other", None, T0), HookOutcome::Accepted(_)),
            "per hook"
        );
        assert!(matches!(
            send(&l, TOKEN, None, T0 + HOOK_INTERVAL_MS),
            HookOutcome::Accepted(_)
        ));
        let h = l
            .hooks()
            .unwrap()
            .hooks
            .into_iter()
            .find(|h| h.name == "h")
            .unwrap();
        assert_eq!(h.refused["rate_limited"], 1);
    }

    #[test]
    fn caps_hook_events_per_day() {
        let db = Memory::new();
        let l = setup(&db);
        hook(&l, "h", TOKEN);
        db.exec(
            "UPDATE project SET hook_day = ?, hook_day_count = ?",
            vec![T0.div_euclid(DAY_MS).into(), HOOK_EVENTS_PER_DAY.into()],
        )
        .unwrap();
        let HookOutcome::Refused {
            status,
            reason,
            retry_after_ms,
        } = send(&l, TOKEN, None, T0)
        else {
            panic!("over the daily cap")
        };
        assert_eq!((status, reason), (429, "daily_limit"));
        let next_day = (T0.div_euclid(DAY_MS) + 1) * DAY_MS;
        assert_eq!(retry_after_ms, Some(next_day - T0));
        assert!(matches!(
            send(&l, TOKEN, None, next_day),
            HookOutcome::Accepted(_)
        ));
    }

    #[test]
    fn an_expired_project_tells_senders_to_stop() {
        let db = Memory::new();
        let l = setup(&db);
        hook(&l, "h", TOKEN);
        let end = T0 + TRIAL_DURATION_MS;
        assert_eq!(
            send(&l, TOKEN, None, end),
            HookOutcome::Refused {
                status: 410,
                reason: "project_expired",
                retry_after_ms: None
            }
        );
        assert!(events(&l).is_empty());
        let e = l
            .put_hook(&spec(serde_json::json!({ "name": "late" })), "hk_late", end)
            .unwrap_err();
        assert_eq!(e.code, "trial_expired");
    }

    #[test]
    fn a_kept_body_goes_with_captured_responses() {
        let db = Memory::new();
        let l = setup(&db);
        hook(&l, "h", TOKEN);
        send(&l, TOKEN, None, T0);
        l.expire_inbox_responses(T0 + CAPTURE_RETENTION_MS).unwrap();
        let e = &events(&l)[0];
        assert_eq!(e["request"]["body"], serde_json::Value::Null);
        assert_eq!(e["request"]["body_bytes"], 22, "what it was is still said");
    }

    fn verified_hook(l: &Ledger<'_, Memory>, name: &str, token: &str) {
        let s = spec(serde_json::json!({
            "name": name,
            "verify": { "scheme": "github", "secret": "whsec_0123456789abcdef" },
        }));
        l.put_hook(&s, token, T0).unwrap();
    }

    #[test]
    fn an_unverified_hook_has_a_small_day_of_its_own() {
        let db = Memory::new();
        let l = setup(&db);
        hook(&l, "open", TOKEN);
        verified_hook(&l, "signed", "hk_signed");
        db.exec(
            "UPDATE hooks SET day = ?, day_count = ? WHERE name = 'open'",
            vec![
                T0.div_euclid(DAY_MS).into(),
                HOOK_UNVERIFIED_EVENTS_PER_DAY.into(),
            ],
        )
        .unwrap();
        assert!(matches!(
            send(&l, TOKEN, None, T0),
            HookOutcome::Refused {
                reason: "daily_limit",
                ..
            }
        ));
        // A leaked open URL doesn't use up the project's day for the signed hook.
        assert!(matches!(
            send(&l, "hk_signed", Some("d-1"), T0),
            HookOutcome::Accepted(_)
        ));
        let next_day = (T0.div_euclid(DAY_MS) + 1) * DAY_MS;
        assert!(matches!(
            send(&l, TOKEN, None, next_day),
            HookOutcome::Accepted(_)
        ));
    }

    #[test]
    fn hook_events_never_push_out_unread_results() {
        let db = Memory::new();
        let l = setup(&db);
        verified_hook(&l, "h", TOKEN);
        let fill = |event: &str, n: i64| {
            for _ in 0..n {
                l.post(T0, |_| Ok(serde_json::json!({ "event": event })))
                    .unwrap();
            }
        };
        // Full of unread task reports: a hook request is refused rather than evicting one.
        fill("task.finished", MAX_INBOX_EVENTS);
        assert!(matches!(
            send(&l, TOKEN, Some("d-1"), T0),
            HookOutcome::Refused {
                status: 429,
                reason: "inbox_full",
                ..
            }
        ));
        // Once there is a hook event to drop, the oldest hook event goes first.
        db.exec("DELETE FROM inbox WHERE seq = 1", vec![]).unwrap();
        fill("hook.received", 1);
        let first_hook: Vec<SeqRow> = db
            .query("SELECT MAX(seq) AS seq FROM inbox", vec![])
            .unwrap();
        assert!(matches!(
            send(&l, TOKEN, Some("d-2"), T0),
            HookOutcome::Accepted(_)
        ));
        let gone: Vec<Counts> = db
            .query(
                "SELECT COUNT(*) AS total, 0 AS waiting FROM inbox WHERE seq = ?",
                vec![first_hook[0].seq.into()],
            )
            .unwrap();
        assert_eq!(gone[0].total, 0, "the older hook event made room");
        let tasks: Vec<Counts> = db
            .query(
                "SELECT COUNT(*) AS total, 0 AS waiting FROM inbox \
                 WHERE json_extract(event, '$.event') = 'task.finished'",
                vec![],
            )
            .unwrap();
        assert_eq!(
            tasks[0].total,
            MAX_INBOX_EVENTS - 1,
            "no task report was dropped"
        );
    }

    #[test]
    fn reports_truncation_as_the_api_saw_it() {
        let db = Memory::new();
        let l = setup(&db);
        hook(&l, "h", TOKEN);
        let row = l.hook_for_token(TOKEN).unwrap().unwrap();
        let r = Received {
            truncated: true,
            body_bytes: 10,
            ..received(None)
        };
        l.receive_hook(&row, r, || true, T0).unwrap();
        assert_eq!(events(&l)[0]["request"]["truncated"], true);
    }

    fn filtered_hook(l: &Ledger<'_, Memory>) {
        let s = spec(serde_json::json!({
            "name": "ci",
            "accept": [{ "headers": { "x-github-event": ["workflow_run"] } }],
        }));
        l.put_hook(&s, TOKEN, T0).unwrap();
    }

    #[test]
    fn a_filtered_request_is_answered_and_dropped() {
        let db = Memory::new();
        let l = setup(&db);
        l.set_wake(
            serde_json::from_value::<WakeRequest>(serde_json::json!({
                "url": "https://agent.example.com/wake",
            }))
            .unwrap(),
            T0,
        )
        .unwrap();
        filtered_hook(&l);
        let row = l.hook_for_token(TOKEN).unwrap().unwrap();
        assert_eq!(
            l.receive_hook(&row, received(None), || false, T0).unwrap(),
            HookOutcome::Filtered
        );
        assert!(events(&l).is_empty(), "nothing written");
        let wakes: Vec<IdRow> = db
            .query("SELECT id FROM tasks WHERE wake_call = 1", vec![])
            .unwrap();
        assert!(wakes.is_empty(), "nothing woken");
        let day: Vec<DayRow> = db
            .query("SELECT hook_day, hook_day_count FROM project", vec![])
            .unwrap();
        assert_eq!(day[0].hook_day_count, 0, "no day used");
        let h = &l.hooks().unwrap().hooks[0];
        assert_eq!((h.filtered, h.received), (1, 0));
        assert_eq!(
            h.accept.as_ref().unwrap()[0].headers["x-github-event"],
            ["workflow_run"]
        );
        // Nor does it spend the hook's rate limit, so noise can't crowd out a real event.
        let tat: Vec<Counts> = db
            .query(
                "SELECT rate_tat_ms AS total, 0 AS waiting FROM hooks",
                vec![],
            )
            .unwrap();
        assert_eq!(tat[0].total, 0);
        // A flood over the limit is turned away before the filter is even asked.
        db.exec(
            "UPDATE hooks SET rate_tat_ms = ?",
            vec![(T0 + 60 * HOOK_INTERVAL_MS).into()],
        )
        .unwrap();
        let row = l.hook_for_token(TOKEN).unwrap().unwrap();
        let outcome = l
            .receive_hook(
                &row,
                received(None),
                || panic!("asked before the rate limit"),
                T0,
            )
            .unwrap();
        assert!(matches!(
            outcome,
            HookOutcome::Refused {
                reason: "rate_limited",
                ..
            }
        ));
        db.exec("UPDATE hooks SET rate_tat_ms = 0", vec![]).unwrap();
        assert!(matches!(
            send(&l, TOKEN, None, T0),
            HookOutcome::Accepted(_)
        ));
    }

    fn patch(v: serde_json::Value) -> HookPatch {
        serde_json::from_value::<runlater_core::HookUpdate>(v)
            .unwrap()
            .validate()
            .unwrap()
    }

    #[test]
    fn updating_a_hook_keeps_its_url_counts_and_deliveries() {
        let db = Memory::new();
        let l = setup(&db);
        verified_hook(&l, "h", TOKEN);
        let sealed_secret = || -> Option<String> {
            db.0.query_row("SELECT sealed_secret FROM hooks", [], |r| r.get(0))
                .unwrap()
        };
        let before = sealed_secret();
        assert!(before.is_some());
        send(&l, TOKEN, Some("d-1"), T0);
        let v = l
            .update_hook(
                "h",
                patch(serde_json::json!({ "accept": [{ "headers": { "X-Event": ["push"] } }] })),
            )
            .unwrap();
        assert_eq!(v.received, 1, "counts kept");
        assert!(v.verify.is_some(), "verify untouched");
        assert!(v.accept.is_some());
        assert!(l.hook_for_token(TOKEN).unwrap().is_some(), "same URL");
        assert_eq!(
            send(&l, TOKEN, Some("d-1"), T0 + 1_000),
            HookOutcome::Duplicate
        );
        assert_eq!(sealed_secret(), before, "secret untouched");

        let v = l
            .update_hook(
                "h",
                patch(serde_json::json!({ "accept": null, "verify": null })),
            )
            .unwrap();
        assert_eq!((v.accept, v.verify), (None, None));
        assert_eq!(sealed_secret(), None, "the old secret is gone with verify");

        let v = l
            .update_hook(
                "h",
                patch(serde_json::json!({ "verify": { "scheme": "github", "secret": "whsec_new_0123456789" } })),
            )
            .unwrap();
        assert_eq!(v.verify.unwrap().scheme.as_str(), "github");
        assert!(sealed_secret().is_some_and(|s| Some(&s) != before.as_ref()));
        assert_eq!(
            l.update_hook("gone", patch(serde_json::json!({})))
                .unwrap_err()
                .code,
            "hook_not_found"
        );
    }

    #[test]
    fn an_update_cant_keep_the_signature_header() {
        let db = Memory::new();
        let l = setup(&db);
        let s = spec(serde_json::json!({
            "name": "h",
            "verify": { "scheme": "hmac_sha256", "secret": "whsec_0123456789abcdef", "header": "X-Sig" },
        }));
        l.put_hook(&s, TOKEN, T0).unwrap();
        let e = l
            .update_hook("h", patch(serde_json::json!({ "keep_headers": ["X-Sig"] })))
            .unwrap_err();
        assert_eq!((e.status, e.field), (400, Some("keep_headers")));
    }

    #[test]
    fn counts_refusals_the_api_made() {
        let db = Memory::new();
        let l = setup(&db);
        hook(&l, "h", TOKEN);
        l.hook_refused("h", "signature_invalid").unwrap();
        l.hook_refused("h", "signature_invalid").unwrap();
        l.hook_refused("gone", "signature_invalid").unwrap();
        assert_eq!(l.hooks().unwrap().hooks[0].refused["signature_invalid"], 2);
    }

    // -- taking a sender's request --------------------------------------------------------

    const GH_SECRET: &str = "whsec_0123456789abcdef";

    /// HMAC-SHA256, as a sender signs.
    fn hmac(key: &[u8], msg: &[u8]) -> String {
        let mut k = [0u8; 64];
        k[..key.len()].copy_from_slice(key);
        let pad = |b: u8| k.iter().map(|x| x ^ b).collect::<Vec<u8>>();
        let inner = Sha256::new()
            .chain_update(pad(0x36))
            .chain_update(msg)
            .finalize();
        hex::encode(
            Sha256::new()
                .chain_update(pad(0x5c))
                .chain_update(inner)
                .finalize(),
        )
    }

    fn github_hook(l: &Ledger<'_, Memory>) {
        let s = spec(serde_json::json!({
            "name": "gh",
            "verify": { "scheme": "github", "secret": GH_SECRET },
        }));
        l.put_hook(&s, TOKEN, T0).unwrap();
    }

    fn request(body: &[u8], headers: &[(&str, &str)]) -> HookDelivery {
        HookDelivery {
            token: TOKEN.into(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: base64::engine::general_purpose::STANDARD.encode(body),
        }
    }

    #[test]
    fn a_correctly_signed_request_is_written_as_verified() {
        let db = Memory::new();
        let l = setup(&db);
        github_hook(&l);
        let body = br#"{"action":"completed"}"#;
        let sig = format!("sha256={}", hmac(GH_SECRET.as_bytes(), body));
        let reply = l
            .take_hook_request(
                request(
                    body,
                    &[("X-Hub-Signature-256", &sig), ("X-GitHub-Delivery", "d1")],
                ),
                T0,
            )
            .unwrap();
        assert_eq!(reply.status, 202);
        let event = &events(&l)[0];
        assert_eq!(event["verified"], true);
        assert_eq!(event["delivery_id"], "d1");

        let again = request(
            body,
            &[("x-hub-signature-256", &sig), ("x-github-delivery", "d1")],
        );
        assert_eq!(
            l.take_hook_request(again, T0 + 1).unwrap().status,
            200,
            "a retry of the same delivery"
        );
    }

    #[test]
    fn a_bad_signature_is_refused_and_counted() {
        let db = Memory::new();
        let l = setup(&db);
        github_hook(&l);
        let wrong = format!("sha256={}", hmac(b"another-secret", b"{}"));
        let reply = l
            .take_hook_request(request(b"{}", &[("x-hub-signature-256", &wrong)]), T0)
            .unwrap();
        assert_eq!(
            (reply.status, reply.reason),
            (401, Some("signature_invalid"))
        );
        let reply = l.take_hook_request(request(b"{}", &[]), T0).unwrap();
        assert_eq!(
            (reply.status, reply.reason),
            (401, Some("signature_missing"))
        );
        let hook = &l.hooks().unwrap().hooks[0];
        assert_eq!(hook.refused["signature_invalid"], 1);
        assert_eq!(hook.refused["signature_missing"], 1);
        assert!(events(&l).is_empty());
    }

    #[test]
    fn without_the_key_a_verified_hook_asks_the_sender_to_retry() {
        let db = Memory::new();
        let l = setup(&db);
        github_hook(&l);
        let keyless = Ledger::open(&db).unwrap();
        let reply = keyless.take_hook_request(request(b"{}", &[]), T0).unwrap();
        assert_eq!(
            (reply.status, reply.reason),
            (503, Some("secrets_unavailable"))
        );
        // Nor can a verified hook be made, but an unverified one needs no key.
        let verified = spec(serde_json::json!({
            "name": "v",
            "verify": { "scheme": "github", "secret": GH_SECRET },
        }));
        let e = keyless.put_hook(&verified, "hk_other", T0).unwrap_err();
        assert_eq!((e.status, e.code), (503, "secrets_unavailable"));
        hook(&keyless, "gh", TOKEN);
        let reply = keyless.take_hook_request(request(b"{}", &[]), T0).unwrap();
        assert_eq!(reply.status, 202);
    }

    #[test]
    fn unknown_malformed_and_oversized_requests_are_refused_before_any_check() {
        let db = Memory::new();
        let l = setup(&db);
        github_hook(&l);
        let unknown = HookDelivery {
            token: format!("hk_{}", "0".repeat(64)),
            ..request(b"{}", &[])
        };
        assert_eq!(
            l.take_hook_request(unknown, T0).unwrap(),
            HookReply::refused(404, "hook_not_found")
        );
        let garbled = HookDelivery {
            body: "not base64!".into(),
            ..request(b"", &[])
        };
        assert_eq!(
            l.take_hook_request(garbled, T0).unwrap(),
            HookReply::refused(400, "invalid_request")
        );
        let big = vec![b'a'; MAX_HOOK_REQUEST_BYTES + 1];
        assert_eq!(
            l.take_hook_request(request(&big, &[]), T0).unwrap(),
            HookReply::refused(413, "too_large")
        );
    }
}
