//! Verifies the short-lived assertion the gateway attaches to every API call.
//!
//! ```text
//! RunLater-Assertion: base64url(claims JSON) "." base64url(Ed25519 signature)
//! ```
//!
//! The signature covers the first segment's ASCII bytes. Only the gateway holds the
//! signing key; the API is configured with its public key. The project an operation acts
//! on comes from these claims alone, never from the request path or body.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::Signature;
pub use ed25519_dalek::VerifyingKey;
use serde::Deserialize;

pub const HEADER: &str = "RunLater-Assertion";
pub const AUDIENCE: &str = "runlater-api";
/// Longest lifetime the API accepts, regardless of what the gateway asks for.
pub const MAX_LIFETIME_SECS: i64 = 300;
/// Tolerated clock skew between gateway and API.
const SKEW_SECS: i64 = 30;
const MAX_TOKEN_BYTES: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Create the project.
    Project,
    /// Schedule and redrive tasks.
    Schedule,
    /// Read the project, tasks and attempts.
    Read,
    Cancel,
    /// Add bought attempts; only the gateway's payment webhook asks for it.
    Billing,
    /// Set and delete project secrets.
    Secrets,
    /// Hand the ledger a request a sender made to one of the project's hooks. Only the
    /// gateway's public hook route asks for it; no agent is granted it.
    HooksReceive,
}

impl Scope {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "project" => Self::Project,
            "schedule" => Self::Schedule,
            "read" => Self::Read,
            "cancel" => Self::Cancel,
            "billing" => Self::Billing,
            "secrets" => Self::Secrets,
            "hooks.receive" => Self::HooksReceive,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claims {
    pub project_id: String,
    pub scopes: Vec<Scope>,
}

impl Claims {
    pub fn allows(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssertionError {
    Malformed,
    BadSignature,
    Expired,
    NotYetValid,
    WrongAudience,
    BadProject,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    aud: String,
    sub: String,
    scope: String,
    iat: i64,
    exp: i64,
}

pub fn parse_public_key(hex_key: &str) -> Option<VerifyingKey> {
    let bytes: [u8; 32] = hex::decode(hex_key.trim()).ok()?.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

pub fn verify(token: &str, key: &VerifyingKey, now_secs: i64) -> Result<Claims, AssertionError> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(AssertionError::Malformed);
    }
    let (payload_b64, sig_b64) = token.split_once('.').ok_or(AssertionError::Malformed)?;
    let sig: [u8; 64] = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or(AssertionError::Malformed)?;
    key.verify_strict(payload_b64.as_bytes(), &Signature::from_bytes(&sig))
        .map_err(|_| AssertionError::BadSignature)?;

    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| AssertionError::Malformed)?;
    let raw: Raw = serde_json::from_slice(&payload).map_err(|_| AssertionError::Malformed)?;
    if raw.aud != AUDIENCE {
        return Err(AssertionError::WrongAudience);
    }
    if raw.iat > now_secs + SKEW_SECS {
        return Err(AssertionError::NotYetValid);
    }
    if raw.exp <= now_secs - SKEW_SECS || raw.exp - raw.iat > MAX_LIFETIME_SECS {
        return Err(AssertionError::Expired);
    }
    if !valid_project_id(&raw.sub) {
        return Err(AssertionError::BadProject);
    }
    let scopes = raw
        .scope
        .split_ascii_whitespace()
        .map(|s| Scope::parse(s).ok_or(AssertionError::Malformed))
        .collect::<Result<_, _>>()?;
    Ok(Claims {
        project_id: raw.sub,
        scopes,
    })
}

pub fn valid_project_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    pub const NOW: i64 = 1_767_225_600;

    pub fn key() -> SigningKey {
        SigningKey::from_bytes(&[7; 32])
    }

    pub fn token(key: &SigningKey, claims: serde_json::Value) -> String {
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        let sig = key.sign(payload.as_bytes());
        format!("{payload}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()))
    }

    fn claims() -> serde_json::Value {
        serde_json::json!({
            "aud": AUDIENCE, "sub": "proj_1", "scope": "read schedule",
            "iat": NOW, "exp": NOW + 60
        })
    }

    fn check(claims: serde_json::Value) -> Result<Claims, AssertionError> {
        verify(&token(&key(), claims), &key().verifying_key(), NOW)
    }

    #[test]
    fn accepts_a_valid_assertion() {
        let c = check(claims()).unwrap();
        assert_eq!(c.project_id, "proj_1");
        assert!(c.allows(Scope::Read) && c.allows(Scope::Schedule));
        assert!(!c.allows(Scope::Cancel));
    }

    /// Signed by apps/gateway's `signAssertion` with seed `[7; 32]`; pinned in
    /// apps/gateway/test/api.test.ts so the two sides cannot drift apart.
    const GATEWAY_TOKEN: &str = "eyJhdWQiOiJydW5sYXRlci1hcGkiLCJzdWIiOiJwcmpfMSIsInNjb3BlIjoicmVhZCBzY2hlZHVsZSIsImlhdCI6MTc2NzIyNTYwMCwiZXhwIjoxNzY3MjI1NjYwfQ.M-yCfOuEo_oLgifHXheOnecRoRyAjPf8bOIvfB40Nn5gBX1nYyubLruWjKOqCQSJXjcv81ogOT6yvv0MbHNECQ";

    #[test]
    fn accepts_a_token_signed_by_the_gateway() {
        let c = verify(GATEWAY_TOKEN, &key().verifying_key(), NOW).unwrap();
        assert_eq!(c.project_id, "prj_1");
        assert_eq!(c.scopes, [Scope::Read, Scope::Schedule]);
    }

    #[test]
    fn parses_hex_public_keys() {
        let hex_key = hex::encode(key().verifying_key().to_bytes());
        assert_eq!(parse_public_key(&hex_key), Some(key().verifying_key()));
        assert_eq!(parse_public_key("abcd"), None);
    }

    #[test]
    fn rejects_other_signers_and_tampering() {
        let other = SigningKey::from_bytes(&[9; 32]);
        let t = token(&other, claims());
        assert_eq!(
            verify(&t, &key().verifying_key(), NOW),
            Err(AssertionError::BadSignature)
        );

        let t = token(&key(), claims());
        let (_, sig) = t.split_once('.').unwrap();
        let mut forged = claims();
        forged["sub"] = "proj_2".into();
        let forged = format!("{}.{sig}", URL_SAFE_NO_PAD.encode(forged.to_string()));
        assert_eq!(
            verify(&forged, &key().verifying_key(), NOW),
            Err(AssertionError::BadSignature)
        );
    }

    #[test]
    fn enforces_time_bounds() {
        let mut c = claims();
        c["exp"] = (NOW - 31).into();
        c["iat"] = (NOW - 90).into();
        assert_eq!(check(c), Err(AssertionError::Expired));

        let mut c = claims();
        c["exp"] = (NOW + MAX_LIFETIME_SECS + 1).into();
        assert_eq!(check(c), Err(AssertionError::Expired));

        let mut c = claims();
        c["iat"] = (NOW + 60).into();
        c["exp"] = (NOW + 120).into();
        assert_eq!(check(c), Err(AssertionError::NotYetValid));
    }

    #[test]
    fn rejects_bad_claims() {
        let mut c = claims();
        c["aud"] = "someone-else".into();
        assert_eq!(check(c), Err(AssertionError::WrongAudience));

        let mut c = claims();
        c["sub"] = "../other".into();
        assert_eq!(check(c), Err(AssertionError::BadProject));

        let mut c = claims();
        c["scope"] = "read admin".into();
        assert_eq!(check(c), Err(AssertionError::Malformed));

        let mut c = claims();
        c["extra"] = true.into();
        assert_eq!(check(c), Err(AssertionError::Malformed));
    }

    #[test]
    fn rejects_garbage() {
        let k = key().verifying_key();
        for t in ["", "abc", "a.b", &"x".repeat(MAX_TOKEN_BYTES + 1)] {
            assert!(verify(t, &k, NOW).is_err(), "{t:?}");
        }
    }
}
