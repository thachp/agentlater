//! Project secrets, and everything the ledger seals with the deployment's keys.
//!
//! A value is sealed as it is stored and never returned: a task carries the sealed value to the
//! delivery consumer, which opens it. A hook's verify secret is sealed the same way under a name
//! of its own, and opened here to check a signature. Rotating the key is
//! docs/adr/0001-rotating-secrets-key.md.

use super::*;
use crate::sealing::{Keyring, Resealed, NONCE_BYTES};
use runlater_core::SecretSpec;

/// What the ledger seals and opens with: the deployment's keys and a source of nonces.
pub struct Sealer {
    pub keys: Keyring,
    /// Fresh random bytes on every call; None when secure randomness is unavailable.
    pub nonce: fn() -> Option<[u8; NONCE_BYTES]>,
}

impl Sealer {
    fn fresh_nonce(&self) -> Result<[u8; NONCE_BYTES], DbError> {
        (self.nonce)().ok_or_else(|| DbError("secure randomness unavailable".into()))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SealedSecret {
    pub name: String,
    pub sealed: String,
}

/// A secret as callers see it: never its value.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct SecretView {
    pub name: String,
    pub allowed_hosts: Vec<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct SecretList {
    pub secrets: Vec<SecretView>,
}

/// What a reseal found: values sealed again under the current key, and values no key opens.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ResealCount {
    /// The keys' name for their rotation ([`Keyring::rotation`]); None when none is on.
    pub rotation: Option<String>,
    pub resealed: usize,
    pub unreadable: usize,
}

#[derive(Deserialize)]
struct SecretRow {
    name: String,
    sealed: String,
    /// The value's length, so headers can be sized as they will be sent.
    value_bytes: i64,
    allowed_hosts: String,
    created_ms: i64,
    updated_ms: i64,
}

impl SecretRow {
    fn hosts(&self) -> Result<Vec<String>, DbError> {
        serde_json::from_str(&self.allowed_hosts).map_err(|e| DbError(e.to_string()))
    }

    fn view(&self) -> Result<SecretView, DbError> {
        Ok(SecretView {
            name: self.name.clone(),
            allowed_hosts: self.hosts()?,
            created_ms: self.created_ms,
            updated_ms: self.updated_ms,
        })
    }
}

/// Why headers may not use a secret they name.
enum Refused {
    Missing(String),
    Host { name: String, allows: Vec<String> },
    TooLarge,
}

/// The name a hook's verify secret is sealed under. Secret names are `A-Z0-9_`, so it can't
/// be mistaken for (or opened as) a project secret.
fn hook_secret_name(hook: &str) -> String {
    format!("hook:{hook}")
}

/// Header bytes as they will be sent: each reference counted at its secret's length.
fn filled_header_bytes(headers: &[(String, String)], secrets: &[SecretRow]) -> usize {
    let pad = "x".repeat(runlater_core::limits::MAX_SECRET_BYTES);
    headers
        .iter()
        .map(|(name, value)| {
            let filled = fill(value, |n| {
                let row = secrets.iter().find(|s| s.name == n)?;
                pad.get(..usize::try_from(row.value_bytes).ok()?)
            });
            name.len() + filled.map_or(value.len(), |f| f.len())
        })
        .sum()
}

impl<D: Db> Ledger<'_, D> {
    /// The deployment's keys, or 503: `what` "are not configured on this deployment".
    fn sealer(&self, what: &str) -> Result<&Sealer, ApiError> {
        self.sealer.as_ref().ok_or_else(|| {
            ApiError::new(
                503,
                "secrets_unavailable",
                format!("{what} are not configured on this deployment"),
            )
        })
    }

    /// Seals a hook's verify secret.
    pub(super) fn seal_hook_secret(
        &self,
        project_id: &str,
        hook: &str,
        secret: &str,
    ) -> Result<String, ApiError> {
        let sealer = self.sealer("verified hooks")?;
        let name = hook_secret_name(hook);
        Ok(sealer
            .keys
            .seal(project_id, &name, secret, sealer.fresh_nonce()?))
    }

    /// A hook's verify secret; None when this deployment has no key that opens it.
    pub(super) fn open_hook_secret(
        &self,
        hook: &str,
        sealed: &str,
    ) -> Result<Option<String>, DbError> {
        let (Some(sealer), Some(project)) = (&self.sealer, self.project_row()?) else {
            return Ok(None);
        };
        Ok(sealer
            .keys
            .open(&project.id, &hook_secret_name(hook), sealed))
    }

    fn secret_row(&self, name: &str) -> Result<Option<SecretRow>, DbError> {
        Ok(self
            .db
            .query(
                "SELECT name, sealed, value_bytes, allowed_hosts, created_ms, updated_ms FROM secrets \
                 WHERE name = ?",
                vec![name.into()],
            )?
            .pop())
    }

    /// Seals and stores a secret, replacing one of the same name. Work that refers to it uses
    /// the new value from its next attempt.
    pub fn put_secret(&self, spec: &SecretSpec, now: i64) -> Result<(SecretView, bool), ApiError> {
        let project = self.project_row()?.ok_or_else(no_project)?;
        let existing = self.secret_row(&spec.name)?;
        if existing.is_none() {
            let count: Vec<Counts> = self.db.query(
                "SELECT COUNT(*) AS total, 0 AS waiting FROM secrets",
                vec![],
            )?;
            if count.first().map_or(0, |c| c.total) >= MAX_SECRETS {
                return Err(ApiError::new(
                    429,
                    "secret_limit",
                    format!("this project already has {MAX_SECRETS} secrets; delete one first"),
                ));
            }
        }
        let sealer = self.sealer("secrets")?;
        let sealed = sealer
            .keys
            .seal(&project.id, &spec.name, &spec.value, sealer.fresh_nonce()?);
        let hosts =
            serde_json::to_string(&spec.allowed_hosts).map_err(|e| DbError(e.to_string()))?;
        self.db.exec(
            "INSERT INTO secrets (name, sealed, value_bytes, allowed_hosts, created_ms, updated_ms) \
             VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (name) DO UPDATE SET sealed = excluded.sealed, \
             value_bytes = excluded.value_bytes, allowed_hosts = excluded.allowed_hosts, \
             updated_ms = excluded.updated_ms",
            vec![
                spec.name.as_str().into(),
                sealed.into(),
                (spec.value.len() as i64).into(),
                hosts.into(),
                now.into(),
                now.into(),
            ],
        )?;
        let row = self
            .secret_row(&spec.name)?
            .ok_or_else(|| DbError("secret vanished".into()))?;
        Ok((row.view()?, existing.is_none()))
    }

    pub fn secrets(&self) -> Result<SecretList, ApiError> {
        self.project_row()?.ok_or_else(no_project)?;
        let rows: Vec<SecretRow> = self.db.query(
            "SELECT name, sealed, value_bytes, allowed_hosts, created_ms, updated_ms FROM secrets ORDER BY name",
            vec![],
        )?;
        let secrets = rows.iter().map(SecretRow::view).collect::<Result<_, _>>()?;
        Ok(SecretList { secrets })
    }

    /// Removes a secret. Work that still refers to it fails, without an attempt, when it next
    /// comes due (`failure: secret_missing`).
    pub fn delete_secret(&self, name: &str) -> Result<SecretView, ApiError> {
        let row = self.secret_row(name)?.ok_or_else(|| {
            ApiError::not_found(
                "secret_not_found",
                "no secret with that name in this project",
            )
        })?;
        self.db
            .exec("DELETE FROM secrets WHERE name = ?", vec![name.into()])?;
        Ok(row.view()?)
    }

    /// Seals again, under the current key, every stored value only the previous key opens:
    /// project secrets and hook verify secrets. Unreadable values are counted and left as they
    /// are.
    pub fn reseal(&self) -> Result<ResealCount, ApiError> {
        #[derive(Deserialize)]
        struct Sealed {
            name: String,
            sealed: String,
        }
        let sealer = self.sealer("secrets")?;
        let mut count = ResealCount {
            rotation: sealer.keys.rotation(),
            ..Default::default()
        };
        // Nothing is sealed before there is a project.
        let Some(project) = self.project_row()? else {
            return Ok(count);
        };
        // (rows, sealed under which name, write back).
        let secret_name: fn(&str) -> String = |name| name.to_string();
        let hook_name: fn(&str) -> String = hook_secret_name;
        let tables = [
            (
                "SELECT name, sealed FROM secrets",
                secret_name,
                "UPDATE secrets SET sealed = ? WHERE name = ? AND sealed = ?",
            ),
            (
                "SELECT name, sealed_secret AS sealed FROM hooks WHERE sealed_secret IS NOT NULL",
                hook_name,
                "UPDATE hooks SET sealed_secret = ? WHERE name = ? AND sealed_secret = ?",
            ),
        ];
        for (select, sealed_as, update) in tables {
            for row in self.db.query::<Sealed>(select, vec![])? {
                let name = sealed_as(&row.name);
                let nonce = || sealer.fresh_nonce();
                match sealer.keys.reseal(&project.id, &name, &row.sealed, nonce)? {
                    Resealed::Current => {}
                    Resealed::Unreadable => count.unreadable += 1,
                    Resealed::Again(again) => {
                        self.db.exec(
                            update,
                            vec![again.into(), row.name.into(), row.sealed.into()],
                        )?;
                        count.resealed += 1;
                    }
                }
            }
        }
        Ok(count)
    }

    /// The one rule for sending secrets: every secret the headers name exists and allows
    /// `host`, and the headers still fit once filled in. This is what stops a tricked agent
    /// from sending a secret somewhere else.
    fn secrets_for(
        &self,
        headers: &[(String, String)],
        names: Vec<String>,
        host: &str,
    ) -> Result<Result<Vec<SecretRow>, Refused>, DbError> {
        if names.is_empty() {
            return Ok(Ok(vec![]));
        }
        let mut secrets = Vec::with_capacity(names.len());
        for name in names {
            let Some(row) = self.secret_row(&name)? else {
                return Ok(Err(Refused::Missing(name)));
            };
            let allows = row.hosts()?;
            if !host_allowed(&allows, host) {
                return Ok(Err(Refused::Host { name, allows }));
            }
            secrets.push(row);
        }
        if filled_header_bytes(headers, &secrets) > MAX_HEADER_BYTES {
            return Ok(Err(Refused::TooLarge));
        }
        Ok(Ok(secrets))
    }

    /// New work may refer only to secrets it may send ([`Self::secrets_for`]).
    pub(super) fn check_secrets(
        &self,
        headers: &[(String, String)],
        host: &str,
    ) -> Result<(), ApiError> {
        let in_headers = |e: ApiError| e.in_field("headers");
        let names =
            header_references(headers).map_err(|e| in_headers(ApiError::bad_request(e.0)))?;
        let Err(refused) = self.secrets_for(headers, names, host)? else {
            return Ok(());
        };
        Err(match refused {
            Refused::Missing(name) => in_headers(ApiError::new(
                400,
                "secret_not_found",
                format!(
                    "headers refer to {{{{secret:{name}}}}}, but this project has no secret {name}"
                ),
            )),
            Refused::Host { name, allows } => ApiError::new(
                400,
                "secret_host_not_allowed",
                format!(
                    "secret {name} may not be sent to {host}; it allows {}",
                    allows.join(", ")
                ),
            )
            .in_field("url"),
            Refused::TooLarge => in_headers(ApiError::bad_request(format!(
                "headers exceed {MAX_HEADER_BYTES} bytes once their secrets are filled in"
            ))),
        })
    }

    /// The sealed secrets a task's headers need, or why it can't be sent: a secret is gone
    /// (`secret_missing`), no longer allows its host (`secret_host_not_allowed`), was replaced by
    /// one too long for its headers (`secret_too_large`), or the API can't open secrets at all
    /// (`secret_unavailable`). Storage errors are errors, not reasons to fail the task.
    pub(super) fn sealed_for(
        &self,
        row: &TaskRow,
    ) -> Result<Result<Vec<SealedSecret>, &'static str>, DbError> {
        let headers = row.headers()?;
        let Ok(names) = header_references(&headers) else {
            return Ok(Err("secret_missing"));
        };
        if names.is_empty() {
            return Ok(Ok(vec![]));
        }
        if self.sealer.is_none() {
            return Ok(Err("secret_unavailable"));
        }
        let Ok(destination) = runlater_egress::Destination::parse(&row.url) else {
            return Ok(Err("secret_host_not_allowed"));
        };
        Ok(
            match self.secrets_for(&headers, names, destination.host())? {
                Ok(secrets) => Ok(secrets
                    .into_iter()
                    .map(|s| SealedSecret {
                        name: s.name,
                        sealed: s.sealed,
                    })
                    .collect()),
                Err(Refused::Missing(_)) => Err("secret_missing"),
                Err(Refused::Host { .. }) => Err("secret_host_not_allowed"),
                Err(Refused::TooLarge) => Err("secret_too_large"),
            },
        )
    }
}

/// A sealer for tests: a fixed key and nonce, so what it seals can be predicted.
#[cfg(test)]
pub(super) fn test_sealer() -> Sealer {
    Sealer {
        keys: Keyring::new(tests::key(0x11), None),
        nonce: || Some([7; NONCE_BYTES]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::Memory;
    use crate::sealing::SealingKey;
    use runlater_core::HookRequest;

    const T0: i64 = 1_767_225_600_000;
    const P: &str = "proj_1";

    pub fn key(byte: u8) -> SealingKey {
        SealingKey::parse(&hex::encode([byte; 32])).unwrap()
    }

    fn ledger(db: &Memory, current: u8, previous: Option<u8>) -> Ledger<'_, Memory> {
        Ledger::open(db).unwrap().with_sealer(Some(Sealer {
            keys: Keyring::new(key(current), previous.map(key)),
            nonce: || Some([7; NONCE_BYTES]),
        }))
    }

    fn put(l: &Ledger<'_, Memory>, name: &str) {
        let spec = SecretSpec {
            name: name.into(),
            value: format!("value of {name}"),
            allowed_hosts: vec!["api.github.com".into()],
        };
        l.put_secret(&spec, T0).unwrap();
    }

    #[test]
    fn nothing_is_sealed_without_keys() {
        let db = Memory::new();
        let l = Ledger::open(&db).unwrap();
        l.create_project(P, "secret", T0).unwrap();
        let spec = SecretSpec {
            name: "GH".into(),
            value: "v".into(),
            allowed_hosts: vec!["api.github.com".into()],
        };
        let e = l.put_secret(&spec, T0).unwrap_err();
        assert_eq!((e.status, e.code), (503, "secrets_unavailable"));
        assert_eq!(l.reseal().unwrap_err().status, 503);
        assert!(l.secrets().unwrap().secrets.is_empty());
    }

    /// Resealing rewrites what only the old key opens, in both tables, under the name each was
    /// sealed with, and leaves current and unreadable values as they are.
    #[test]
    fn reseals_secrets_and_hook_secrets() {
        let db = Memory::new();
        let (old, new, lost) = (1, 2, 3);
        let l = ledger(&db, old, None);
        l.create_project(P, "secret", T0).unwrap();
        put(&l, "OLD");
        let hook = |name: &str, verify: bool| {
            let mut v = serde_json::json!({ "name": name });
            if verify {
                v["verify"] =
                    serde_json::json!({ "scheme": "github", "secret": "whsec_0123456789abcdef" });
            }
            let spec = serde_json::from_value::<HookRequest>(v)
                .unwrap()
                .validate()
                .unwrap();
            l.put_hook(&spec, &format!("hk_{name}"), T0).unwrap();
        };
        hook("gh-ci", true);
        hook("plain", false);
        put(&ledger(&db, new, None), "NEW");
        put(&ledger(&db, lost, None), "LOST");
        // No rotation on: nothing to reseal, and what this key can't open is only counted.
        let idle = l.reseal().unwrap();
        assert_eq!(
            (idle.rotation, idle.resealed, idle.unreadable),
            (None, 0, 2)
        );

        let rotating = ledger(&db, new, Some(old));
        let count = rotating.reseal().unwrap();
        assert_eq!((count.resealed, count.unreadable), (2, 1));
        assert_eq!(
            count.rotation,
            Keyring::new(key(new), Some(key(old))).rotation()
        );
        assert!(count.rotation.is_some());

        let stored = |sql: &str| -> String { db.0.query_row(sql, [], |r| r.get(0)).unwrap() };
        let opens = |k: u8, name: &str, sql: &str| key(k).open(P, name, &stored(sql));
        assert_eq!(
            opens(new, "OLD", "SELECT sealed FROM secrets WHERE name = 'OLD'").as_deref(),
            Some("value of OLD")
        );
        assert_eq!(
            opens(
                new,
                "hook:gh-ci",
                "SELECT sealed_secret FROM hooks WHERE name = 'gh-ci'"
            )
            .as_deref(),
            Some("whsec_0123456789abcdef"),
            "under the hook's own name"
        );
        assert!(opens(new, "NEW", "SELECT sealed FROM secrets WHERE name = 'NEW'").is_some());
        assert!(
            opens(
                lost,
                "LOST",
                "SELECT sealed FROM secrets WHERE name = 'LOST'"
            )
            .is_some(),
            "left as it was"
        );
        let again = rotating.reseal().unwrap();
        assert_eq!(
            (again.resealed, again.unreadable),
            (0, 1),
            "nothing left to reseal"
        );
    }

    #[test]
    fn a_ledger_without_a_project_has_nothing_to_reseal() {
        let db = Memory::new();
        let count = ledger(&db, 2, Some(1)).reseal().unwrap();
        assert_eq!((count.resealed, count.unreadable), (0, 0));
        assert!(count.rotation.is_some(), "the sweep still learns the keys");
    }
}
