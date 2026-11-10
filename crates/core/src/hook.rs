//! Inbound hooks: a URL a project gives to a sender (GitHub, Stripe, a CI system) so its
//! webhooks land in the project's inbox.
//!
//! This module checks what an agent asks for ([`HookRequest`]), checks a sender's signature
//! ([`verify`]) and decides what of a request is kept ([`kept_headers`], [`delivery_id`]).
//! Tokens, storage, rate limits and the inbox are the ledger's.

use crate::cron::valid_cron_name;
use crate::limits::{
    HOOK_SIGNATURE_TOLERANCE_MS, MAX_HOOK_SECRET_BYTES, MAX_KEEP_HEADERS, MIN_HOOK_SECRET_BYTES,
};
use crate::request::FieldError;
use crate::watch::Until;
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::fmt;

/// Kept on every event: they say what sent it and what the body is.
const ALWAYS_KEPT: [&str; 2] = ["content-type", "user-agent"];
/// Never kept, whatever `keep_headers` says: they carry credentials (GitLab, for one, sends
/// its shared secret as `X-Gitlab-Token`).
const NEVER_KEPT: [&str; 8] = [
    "authorization",
    "cookie",
    "proxy-authorization",
    "set-cookie",
    "x-api-key",
    "x-auth-token",
    "x-access-token",
    "x-gitlab-token",
];
const MAX_KEPT_VALUE_BYTES: usize = 1024;
const MAX_HEADER_NAME_BYTES: usize = 64;
const MAX_PREFIX_BYTES: usize = 32;
const MAX_DELIVERY_ID_BYTES: usize = 128;
const MAX_ACCEPT_RULES: usize = 5;
const MAX_RULE_HEADERS: usize = 5;
const MAX_RULE_VALUES: usize = 20;
const MAX_RULE_VALUE_BYTES: usize = 256;

/// How a sender signs its requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scheme {
    /// `X-Hub-Signature-256: sha256=<hex HMAC-SHA256 of the body>`.
    Github,
    /// `Stripe-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256 of "t.body">`, within 5 minutes.
    Stripe,
    /// A header you name holding the HMAC-SHA256 of the body, hex or base64, after `prefix`.
    HmacSha256,
}

impl Scheme {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Stripe => "stripe",
            Self::HmacSha256 => "hmac_sha256",
        }
    }

    /// The header the signature is in, if the scheme fixes it.
    fn signature_header(&self) -> Option<&'static str> {
        match self {
            Self::Github => Some("x-hub-signature-256"),
            Self::Stripe => Some("stripe-signature"),
            Self::HmacSha256 => None,
        }
    }

    /// Headers the scheme's sender uses to say what happened, kept on every event.
    fn event_headers(&self) -> &'static [&'static str] {
        match self {
            Self::Github => &["x-github-event", "x-github-delivery", "x-github-hook-id"],
            Self::Stripe | Self::HmacSha256 => &[],
        }
    }
}

/// `create_hook` input exactly as the caller sent it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookRequest {
    pub name: String,
    #[serde(default)]
    pub verify: Option<VerifyRequest>,
    /// More request headers to keep on each event, e.g. `"x-gitlab-event"`.
    #[serde(default)]
    pub keep_headers: Vec<String>,
    /// Keep only requests that match one of these rules; the rest are answered and dropped.
    #[serde(default)]
    pub accept: Option<Vec<AcceptRule>>,
}

/// One way a request can be worth keeping. Every part given must hold.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptRule {
    /// Header name to the values that match it, compared exactly. Names are stored lowercase.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, Vec<String>>,
    /// With `in`: a JSON Pointer into the body, as a watch's `until`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, rename = "in", skip_serializing_if = "Option::is_none")]
    pub one_of: Option<Vec<Value>>,
}

/// `update_hook` input: what to change; anything left out stays. `null` clears `verify` or
/// `accept`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookUpdate {
    #[serde(default, deserialize_with = "present")]
    pub verify: Option<Option<VerifyRequest>>,
    #[serde(default, deserialize_with = "present")]
    pub accept: Option<Option<Vec<AcceptRule>>>,
    /// `null` keeps only the headers always kept, as `[]` does.
    #[serde(default, deserialize_with = "present")]
    pub keep_headers: Option<Option<Vec<String>>>,
}

/// Tells a field given as `null` (`Some(None)`) from one left out (`None`).
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// A validated `update_hook`. `keep_headers` is checked against the hook's signature header
/// by [`check_keep`] once the ledger knows which `verify` applies.
#[derive(Clone, PartialEq, Eq)]
pub struct HookPatch {
    pub verify: Option<Option<(VerifySpec, String)>>,
    pub accept: Option<Option<Vec<AcceptRule>>>,
    pub keep_headers: Option<Vec<String>>,
}

impl fmt::Debug for HookPatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookPatch")
            .field(
                "verify",
                &self.verify.as_ref().map(|v| v.as_ref().map(|(s, _)| s)),
            )
            .field("accept", &self.accept)
            .field("keep_headers", &self.keep_headers)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyRequest {
    pub scheme: Scheme,
    /// The signing secret configured at the sender. Sealed by the API, never shown again.
    pub secret: String,
    /// For `hmac_sha256`: the header carrying the signature.
    #[serde(default)]
    pub header: Option<String>,
    /// For `hmac_sha256`: text before the signature, e.g. `"sha256="`.
    #[serde(default)]
    pub prefix: Option<String>,
}

/// Never shows the secret, so logging a request can't leak it.
impl fmt::Debug for VerifyRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerifyRequest")
            .field("scheme", &self.scheme)
            .field("secret", &"[redacted]")
            .field("header", &self.header)
            .field("prefix", &self.prefix)
            .finish()
    }
}

/// How a hook checks signatures, as stored. The secret is stored apart, sealed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifySpec {
    pub scheme: Scheme,
    /// Lowercase. Set for `hmac_sha256` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
}

impl VerifySpec {
    fn header(&self) -> &str {
        self.scheme
            .signature_header()
            .or(self.header.as_deref())
            .unwrap_or_default()
    }
}

/// A validated hook, ready to store.
#[derive(Clone, PartialEq, Eq)]
pub struct HookSpec {
    pub name: String,
    /// The check, and the secret to seal for it.
    pub verify: Option<(VerifySpec, String)>,
    /// Lowercase, sorted, without duplicates.
    pub keep_headers: Vec<String>,
    pub accept: Option<Vec<AcceptRule>>,
}

impl fmt::Debug for HookSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookSpec")
            .field("name", &self.name)
            .field("verify", &self.verify.as_ref().map(|(v, _)| v))
            .field("keep_headers", &self.keep_headers)
            .field("accept", &self.accept)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookError {
    Name,
    SecretLength,
    SecretChars,
    /// `header` is required for `hmac_sha256` and not allowed otherwise.
    HeaderUse(Scheme),
    Header(String),
    PrefixUse,
    Prefix,
    TooManyKept,
    /// Not a header name, or one that is never kept.
    Kept(String),
    Accept(String),
}

impl FieldError for HookError {
    fn field(&self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::SecretLength
            | Self::SecretChars
            | Self::HeaderUse(_)
            | Self::Header(_)
            | Self::PrefixUse
            | Self::Prefix => "verify",
            Self::TooManyKept | Self::Kept(_) => "keep_headers",
            Self::Accept(_) => "accept",
        }
    }
}

impl fmt::Display for HookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name => f.write_str(
                "name must be 1-64 characters of A-Z a-z 0-9 _ - . :, starting with a letter or digit",
            ),
            Self::SecretLength => write!(
                f,
                "verify.secret must be {MIN_HOOK_SECRET_BYTES}-{MAX_HOOK_SECRET_BYTES} bytes"
            ),
            Self::SecretChars => f.write_str("verify.secret must be printable ASCII"),
            Self::HeaderUse(Scheme::HmacSha256) => {
                f.write_str("verify.header is required with scheme hmac_sha256")
            }
            Self::HeaderUse(s) => write!(
                f,
                "verify.header is only for hmac_sha256; {} always uses {}",
                s.as_str(),
                s.signature_header().unwrap_or_default()
            ),
            Self::Header(h) => write!(f, "verify.header {h:?} is not a header name AgentLater can check"),
            Self::PrefixUse => f.write_str("verify.prefix is only for hmac_sha256"),
            Self::Prefix => write!(
                f,
                "verify.prefix must be at most {MAX_PREFIX_BYTES} printable ASCII characters"
            ),
            Self::TooManyKept => write!(f, "keep_headers lists at most {MAX_KEEP_HEADERS} names"),
            Self::Kept(h) => write!(
                f,
                "keep_headers: {h:?} is not a header name, or carries credentials and is never kept"
            ),
            Self::Accept(why) => write!(f, "accept: {why}"),
        }
    }
}

impl std::error::Error for HookError {}

fn printable(s: &str) -> bool {
    s.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// RFC 9110 token characters.
fn header_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_HEADER_NAME_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

impl HookRequest {
    pub fn validate(self) -> Result<HookSpec, HookError> {
        if !valid_cron_name(&self.name) {
            return Err(HookError::Name);
        }
        let verify = self.verify.map(VerifyRequest::validate).transpose()?;
        let keep_headers = check_keep(verify.as_ref().map(|(v, _)| v), self.keep_headers)?;
        Ok(HookSpec {
            name: self.name,
            verify,
            keep_headers,
            accept: self.accept.map(check_accept).transpose()?,
        })
    }
}

impl HookUpdate {
    pub fn validate(self) -> Result<HookPatch, HookError> {
        Ok(HookPatch {
            verify: self
                .verify
                .map(|v| v.map(VerifyRequest::validate).transpose())
                .transpose()?,
            accept: self
                .accept
                .map(|a| a.map(check_accept).transpose())
                .transpose()?,
            keep_headers: self.keep_headers.map(Option::unwrap_or_default),
        })
    }
}

/// `keep_headers`, lowercase, sorted and deduplicated: never credentials, nor the header
/// `verify` reads the signature from.
pub fn check_keep(
    verify: Option<&VerifySpec>,
    names: Vec<String>,
) -> Result<Vec<String>, HookError> {
    if names.len() > MAX_KEEP_HEADERS {
        return Err(HookError::TooManyKept);
    }
    let signature = verify.map(VerifySpec::header);
    let mut keep = Vec::with_capacity(names.len());
    for name in names {
        let lower = name.to_ascii_lowercase();
        let forbidden = NEVER_KEPT.contains(&lower.as_str())
            || signature == Some(lower.as_str())
            || ["x-hub-signature-256", "x-hub-signature", "stripe-signature"]
                .contains(&lower.as_str());
        if !header_name(&name) || forbidden {
            return Err(HookError::Kept(name));
        }
        keep.push(lower);
    }
    keep.sort();
    keep.dedup();
    Ok(keep)
}

/// Checks `accept` and stores header names lowercase.
fn check_accept(rules: Vec<AcceptRule>) -> Result<Vec<AcceptRule>, HookError> {
    let bad = |why: &str| HookError::Accept(why.into());
    if rules.is_empty() || rules.len() > MAX_ACCEPT_RULES {
        return Err(bad(
            "give 1-5 rules, or leave accept out to keep everything",
        ));
    }
    rules
        .into_iter()
        .map(|rule| {
            if rule.headers.is_empty() && rule.path.is_none() && rule.one_of.is_none() {
                return Err(bad("a rule needs headers, or path and in"));
            }
            if rule.headers.len() > MAX_RULE_HEADERS {
                return Err(bad("a rule names at most 5 headers"));
            }
            let mut headers = BTreeMap::new();
            for (name, values) in rule.headers {
                if !header_name(&name) {
                    return Err(HookError::Accept(format!("{name:?} is not a header name")));
                }
                if values.is_empty()
                    || values.len() > MAX_RULE_VALUES
                    || values.iter().any(|v| v.len() > MAX_RULE_VALUE_BYTES)
                {
                    return Err(bad("each header lists 1-20 values of at most 256 bytes"));
                }
                // Request header values are compared trimmed, so an untrimmed one never matches.
                if values.iter().any(|v| v.trim() != v || v.is_empty()) {
                    return Err(bad(
                        "header values can't be empty or start or end with spaces",
                    ));
                }
                if headers.insert(name.to_ascii_lowercase(), values).is_some() {
                    return Err(HookError::Accept(format!("header {name:?} is given twice")));
                }
            }
            if rule.path.is_some() || rule.one_of.is_some() {
                let until = Until {
                    path: rule.path.clone(),
                    one_of: rule.one_of.clone(),
                    status: None,
                };
                until.check().map_err(|why| HookError::Accept(why.into()))?;
            }
            Ok(AcceptRule { headers, ..rule })
        })
        .collect()
}

/// Whether a request matches one of `rules`: every header listed has one of its values, and
/// a `path` finds one of `in` in the body, read as JSON.
///
/// Headers are checked first, and the body is parsed at most once, only if a rule whose
/// headers match needs it: a sender of large bodies can't make every rule parse them again.
pub fn accepts(rules: &[AcceptRule], headers: &[(String, String)], body: &[u8]) -> bool {
    let mut doc: Option<Option<Value>> = None;
    rules.iter().any(|rule| {
        let headers_match = rule.headers.iter().all(|(name, values)| {
            header(headers, name).is_some_and(|v| values.iter().any(|want| want == v))
        });
        if !headers_match {
            return false;
        }
        if rule.path.is_none() {
            return true;
        }
        let doc = doc.get_or_insert_with(|| serde_json::from_slice(body).ok());
        let until = Until {
            path: rule.path.clone(),
            one_of: rule.one_of.clone(),
            status: None,
        };
        doc.as_ref()
            .is_some_and(|d| until.matches_document(d).is_some())
    })
}

impl VerifyRequest {
    fn validate(self) -> Result<(VerifySpec, String), HookError> {
        if !(MIN_HOOK_SECRET_BYTES..=MAX_HOOK_SECRET_BYTES).contains(&self.secret.len()) {
            return Err(HookError::SecretLength);
        }
        if !printable(&self.secret) {
            return Err(HookError::SecretChars);
        }
        let custom = self.scheme == Scheme::HmacSha256;
        let header = match (custom, self.header) {
            (true, Some(h)) => {
                let lower = h.to_ascii_lowercase();
                if !header_name(&h) || NEVER_KEPT.contains(&lower.as_str()) {
                    return Err(HookError::Header(h));
                }
                Some(lower)
            }
            (false, None) => None,
            _ => return Err(HookError::HeaderUse(self.scheme)),
        };
        let prefix = match (custom, self.prefix) {
            (_, None) => None,
            (true, Some(p)) if p.len() <= MAX_PREFIX_BYTES && printable(&p) => Some(p),
            (true, Some(_)) => return Err(HookError::Prefix),
            (false, Some(_)) => return Err(HookError::PrefixUse),
        };
        Ok((
            VerifySpec {
                scheme: self.scheme,
                header,
                prefix,
            },
            self.secret,
        ))
    }
}

/// Why a request's signature was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// The signature header is absent.
    Missing,
    /// Present but malformed, or it doesn't match.
    Invalid,
    /// It matches a timestamp too far from now: a replay, or a clock far off.
    Stale,
}

impl VerifyError {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Missing => "signature_missing",
            Self::Invalid => "signature_invalid",
            Self::Stale => "signature_stale",
        }
    }
}

/// `headers` have lowercase names; the first of a repeated name counts.
fn header<'h>(headers: &'h [(String, String)], name: &str) -> Option<&'h str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.trim())
}

fn mac(secret: &[u8]) -> Hmac<Sha256> {
    Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length")
}

/// Checks a request's signature with the hook's secret. Comparisons are constant-time.
pub fn verify(
    spec: &VerifySpec,
    secret: &[u8],
    headers: &[(String, String)],
    body: &[u8],
    now_ms: i64,
) -> Result<(), VerifyError> {
    let value = header(headers, spec.header()).ok_or(VerifyError::Missing)?;
    match spec.scheme {
        Scheme::Github => {
            let sig = value
                .strip_prefix("sha256=")
                .and_then(|h| hex::decode(h).ok())
                .ok_or(VerifyError::Invalid)?;
            let mut m = mac(secret);
            m.update(body);
            m.verify_slice(&sig).map_err(|_| VerifyError::Invalid)
        }
        Scheme::Stripe => {
            let mut t = None;
            let mut sigs = Vec::new();
            for part in value.split(',') {
                match part.trim().split_once('=') {
                    Some(("t", v)) => t = v.parse::<i64>().ok(),
                    Some(("v1", v)) => sigs.extend(hex::decode(v).ok()),
                    _ => {}
                }
            }
            let t = t.ok_or(VerifyError::Invalid)?;
            let mut m = mac(secret);
            m.update(format!("{t}.").as_bytes());
            m.update(body);
            if !sigs.iter().any(|s| m.clone().verify_slice(s).is_ok()) {
                return Err(VerifyError::Invalid);
            }
            // Checked after the signature, so only a genuine but old request is called stale.
            let signed_ms = t.checked_mul(1_000).ok_or(VerifyError::Stale)?;
            if now_ms.abs_diff(signed_ms) > HOOK_SIGNATURE_TOLERANCE_MS.unsigned_abs() {
                return Err(VerifyError::Stale);
            }
            Ok(())
        }
        Scheme::HmacSha256 => {
            let encoded = match &spec.prefix {
                Some(p) => value.strip_prefix(p.as_str()).ok_or(VerifyError::Invalid)?,
                None => value,
            };
            let sig = decode_signature(encoded).ok_or(VerifyError::Invalid)?;
            let mut m = mac(secret);
            m.update(body);
            m.verify_slice(&sig).map_err(|_| VerifyError::Invalid)
        }
    }
}

/// 32 bytes as hex, or as base64 (standard or URL-safe).
fn decode_signature(s: &str) -> Option<Vec<u8>> {
    if s.len() == 64 {
        if let Ok(b) = hex::decode(s) {
            return Some(b);
        }
    }
    [&STANDARD, &URL_SAFE, &URL_SAFE_NO_PAD]
        .iter()
        .find_map(|e| e.decode(s).ok())
        .filter(|b| b.len() == 32)
}

/// What an event keeps of a request's headers: `content-type` and `user-agent`, the scheme's
/// event headers, and `keep`. Never credentials or the signature. Values are cut to 1 KiB.
pub fn kept_headers(
    verify: Option<&VerifySpec>,
    keep: &[String],
    headers: &[(String, String)],
) -> BTreeMap<String, String> {
    let scheme_headers = verify.map_or(&[][..], |v| v.scheme.event_headers());
    let wanted = ALWAYS_KEPT
        .iter()
        .chain(scheme_headers)
        .copied()
        .chain(keep.iter().map(String::as_str));
    let signature = verify.map(VerifySpec::header);
    let mut kept = BTreeMap::new();
    for name in wanted {
        if NEVER_KEPT.contains(&name) || Some(name) == signature || kept.contains_key(name) {
            continue;
        }
        if let Some(v) = header(headers, name) {
            kept.insert(name.to_string(), cut(v, MAX_KEPT_VALUE_BYTES).to_string());
        }
    }
    kept
}

/// The sender's id for this delivery, which a retry repeats: GitHub's `X-GitHub-Delivery`,
/// or a Stripe event's `id`.
pub fn delivery_id(
    verify: Option<&VerifySpec>,
    headers: &[(String, String)],
    body: &[u8],
) -> Option<String> {
    let id = match verify?.scheme {
        Scheme::Github => header(headers, "x-github-delivery")?.to_string(),
        Scheme::Stripe => {
            let doc: serde_json::Value = serde_json::from_slice(body).ok()?;
            doc.get("id")?.as_str()?.to_string()
        }
        Scheme::HmacSha256 => return None,
    };
    (!id.is_empty() && id.len() <= MAX_DELIVERY_ID_BYTES && printable(&id)).then_some(id)
}

/// The longest prefix of `s` that fits in `max` bytes, cut on a character boundary.
pub fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_767_225_600_000;
    const SECRET: &str = "whsec_0123456789abcdef";

    fn request(v: serde_json::Value) -> HookRequest {
        serde_json::from_value(v).unwrap()
    }

    fn sign(secret: &str, parts: &[&[u8]]) -> String {
        let mut m = mac(secret.as_bytes());
        for p in parts {
            m.update(p);
        }
        hex::encode(m.finalize().into_bytes())
    }

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn spec(scheme: Scheme) -> VerifySpec {
        VerifySpec {
            scheme,
            header: (scheme == Scheme::HmacSha256).then(|| "x-signature".into()),
            prefix: None,
        }
    }

    #[test]
    fn validates_a_hook() {
        let s = request(json!({
            "name": "gh-ci",
            "verify": { "scheme": "github", "secret": SECRET },
            "keep_headers": ["X-Request-Id", "x-request-id", "X-Custom"],
        }))
        .validate()
        .unwrap();
        assert_eq!(s.keep_headers, ["x-custom", "x-request-id"]);
        let (v, secret) = s.verify.unwrap();
        assert_eq!(
            (v.scheme, v.header, secret.as_str()),
            (Scheme::Github, None, SECRET)
        );

        let plain = request(json!({ "name": "form" })).validate().unwrap();
        assert!(plain.verify.is_none());
        let custom = request(json!({
            "name": "ci",
            "verify": { "scheme": "hmac_sha256", "secret": SECRET, "header": "X-Signature", "prefix": "sha256=" },
        }))
        .validate()
        .unwrap();
        assert_eq!(
            custom.verify.unwrap().0,
            VerifySpec {
                scheme: Scheme::HmacSha256,
                header: Some("x-signature".into()),
                prefix: Some("sha256=".into())
            }
        );
    }

    #[test]
    fn rejects_bad_hooks() {
        let cases = [
            (json!({ "name": "_x" }), "name"),
            (json!({ "name": "a/b" }), "name"),
            (
                json!({ "name": "h", "verify": { "scheme": "github", "secret": "short" } }),
                "verify",
            ),
            (
                json!({ "name": "h", "verify": { "scheme": "github", "secret": "line\nbreak-0123456789" } }),
                "verify",
            ),
            (
                json!({ "name": "h", "verify": { "scheme": "hmac_sha256", "secret": SECRET } }),
                "verify",
            ),
            (
                json!({ "name": "h", "verify": { "scheme": "github", "secret": SECRET, "header": "x-sig" } }),
                "verify",
            ),
            (
                json!({ "name": "h", "verify": { "scheme": "stripe", "secret": SECRET, "prefix": "v1=" } }),
                "verify",
            ),
            (
                json!({ "name": "h", "verify": { "scheme": "hmac_sha256", "secret": SECRET, "header": "bad header" } }),
                "verify",
            ),
            (
                json!({ "name": "h", "verify": { "scheme": "hmac_sha256", "secret": SECRET, "header": "Authorization" } }),
                "verify",
            ),
            (
                json!({ "name": "h", "keep_headers": ["Authorization"] }),
                "keep_headers",
            ),
            (
                json!({ "name": "h", "keep_headers": ["Cookie"] }),
                "keep_headers",
            ),
            (
                json!({ "name": "h", "keep_headers": ["X-Gitlab-Token"] }),
                "keep_headers",
            ),
            (
                json!({ "name": "h", "keep_headers": ["X-Hub-Signature-256"] }),
                "keep_headers",
            ),
            (
                json!({ "name": "h", "keep_headers": ["has space"] }),
                "keep_headers",
            ),
            (
                json!({ "name": "h", "verify": { "scheme": "hmac_sha256", "secret": SECRET, "header": "X-Sig" }, "keep_headers": ["x-sig"] }),
                "keep_headers",
            ),
            (
                json!({ "name": "h", "keep_headers": (0..11).map(|i| format!("x-{i}")).collect::<Vec<_>>() }),
                "keep_headers",
            ),
        ];
        for (input, field) in cases {
            let err = request(input.clone()).validate().unwrap_err();
            assert_eq!(err.field(), field, "{input}: {err}");
        }
        assert!(serde_json::from_value::<HookRequest>(json!({ "name": "h", "url": "x" })).is_err());
        assert!(serde_json::from_value::<HookRequest>(
            json!({ "name": "h", "verify": { "scheme": "md5", "secret": SECRET } })
        )
        .is_err());
    }

    #[test]
    fn never_shows_the_secret() {
        let r = request(json!({ "name": "h", "verify": { "scheme": "github", "secret": SECRET } }));
        assert!(!format!("{r:?}").contains(SECRET));
        assert!(!format!("{:?}", r.validate().unwrap()).contains(SECRET));
    }

    #[test]
    fn matches_a_fixed_hmac_vector() {
        // Same as Node: createHmac("sha256", secret).update(body).digest("hex")
        assert_eq!(
            sign("hook-secret-0123456789", &[br#"{"ok":true}"#]),
            "2c3ffff5c757b038ae1facbd577bfdf43474acc7559feefbc6f53a0d4a0c336d"
        );
    }

    #[test]
    fn checks_github_signatures() {
        let body = br#"{"action":"completed"}"#;
        let good = format!("sha256={}", sign(SECRET, &[body]));
        let check = |h: &[(&str, &str)], b: &[u8]| {
            verify(
                &spec(Scheme::Github),
                SECRET.as_bytes(),
                &headers(h),
                b,
                NOW,
            )
        };
        assert_eq!(check(&[("x-hub-signature-256", &good)], body), Ok(()));
        assert_eq!(check(&[], body), Err(VerifyError::Missing));
        assert_eq!(
            check(&[("x-hub-signature-256", &good)], b"{}"),
            Err(VerifyError::Invalid)
        );
        assert_eq!(
            check(
                &[("x-hub-signature-256", good.trim_start_matches("sha256="))],
                body
            ),
            Err(VerifyError::Invalid)
        );
        assert_eq!(
            check(&[("x-hub-signature-256", "sha256=zz")], body),
            Err(VerifyError::Invalid)
        );
        let other = format!("sha256={}", sign("another-secret-0123", &[body]));
        assert_eq!(
            check(&[("x-hub-signature-256", &other)], body),
            Err(VerifyError::Invalid)
        );
    }

    #[test]
    fn checks_stripe_signatures_and_their_age() {
        let body = br#"{"id":"evt_1","type":"charge.disputed"}"#;
        let t = NOW / 1000;
        let sig = |t: i64| sign(SECRET, &[format!("{t}.").as_bytes(), body]);
        let check = |value: String, now: i64| {
            verify(
                &spec(Scheme::Stripe),
                SECRET.as_bytes(),
                &headers(&[("stripe-signature", &value)]),
                body,
                now,
            )
        };
        assert_eq!(check(format!("t={t},v1={}", sig(t)), NOW), Ok(()));
        // Several v1 entries while Stripe rolls a secret: any one may match.
        assert_eq!(
            check(format!("t={t},v1={},v1={}", "00".repeat(32), sig(t)), NOW),
            Ok(())
        );
        assert_eq!(
            check(format!("t={t},v1={}", sig(t + 1)), NOW),
            Err(VerifyError::Invalid)
        );
        assert_eq!(
            check(format!("v1={}", sig(t)), NOW),
            Err(VerifyError::Invalid)
        );
        assert_eq!(
            check(format!("t={t},v1={}", sig(t)), NOW + 5 * 60_000),
            Ok(())
        );
        assert_eq!(
            check(format!("t={t},v1={}", sig(t)), NOW + 5 * 60_000 + 1_000),
            Err(VerifyError::Stale)
        );
        // A signed but absurd timestamp is stale, not a wrapped-around "now".
        let far = i64::MAX / 10;
        assert_eq!(
            check(format!("t={far},v1={}", sig(far)), NOW),
            Err(VerifyError::Stale)
        );
    }

    #[test]
    fn checks_custom_hmac_signatures_in_hex_or_base64() {
        let body = b"payload";
        let raw = hex::decode(sign(SECRET, &[body])).unwrap();
        let mut with_prefix = spec(Scheme::HmacSha256);
        with_prefix.prefix = Some("sha256=".into());
        let check = |s: &VerifySpec, value: &str| {
            verify(
                s,
                SECRET.as_bytes(),
                &headers(&[("x-signature", value)]),
                body,
                NOW,
            )
        };
        let plain = spec(Scheme::HmacSha256);
        assert_eq!(check(&plain, &hex::encode(&raw)), Ok(()));
        assert_eq!(check(&plain, &STANDARD.encode(&raw)), Ok(()));
        assert_eq!(check(&plain, &URL_SAFE_NO_PAD.encode(&raw)), Ok(()));
        assert_eq!(
            check(&with_prefix, &format!("sha256={}", hex::encode(&raw))),
            Ok(())
        );
        assert_eq!(
            check(&with_prefix, &hex::encode(&raw)),
            Err(VerifyError::Invalid)
        );
        assert_eq!(
            check(&plain, &STANDARD.encode(&raw[..16])),
            Err(VerifyError::Invalid)
        );
    }

    #[test]
    fn keeps_only_allowed_headers() {
        let h = headers(&[
            ("content-type", "application/json"),
            ("user-agent", "GitHub-Hookshot/abc"),
            ("x-github-event", "workflow_run"),
            ("x-github-delivery", "d-1"),
            ("x-hub-signature-256", "sha256=00"),
            ("authorization", "Bearer leaked"),
            ("cookie", "s=1"),
            ("x-custom", &"v".repeat(2000)),
            ("x-other", "dropped"),
        ]);
        let github = spec(Scheme::Github);
        let kept = kept_headers(Some(&github), &["x-custom".into()], &h);
        assert_eq!(
            kept.keys().collect::<Vec<_>>(),
            [
                "content-type",
                "user-agent",
                "x-custom",
                "x-github-delivery",
                "x-github-event"
            ]
        );
        assert_eq!(kept["x-custom"].len(), MAX_KEPT_VALUE_BYTES);
        let unverified = kept_headers(None, &[], &h);
        assert_eq!(
            unverified.keys().collect::<Vec<_>>(),
            ["content-type", "user-agent"]
        );
    }

    #[test]
    fn finds_the_senders_delivery_id() {
        let github = spec(Scheme::Github);
        let stripe = spec(Scheme::Stripe);
        let h = headers(&[("x-github-delivery", "72d3162e")]);
        assert_eq!(
            delivery_id(Some(&github), &h, b"{}").as_deref(),
            Some("72d3162e")
        );
        assert_eq!(delivery_id(Some(&github), &[], b"{}"), None);
        assert_eq!(
            delivery_id(Some(&stripe), &[], br#"{"id":"evt_1"}"#).as_deref(),
            Some("evt_1")
        );
        assert_eq!(delivery_id(Some(&stripe), &[], b"not json"), None);
        assert_eq!(
            delivery_id(None, &h, b"{}"),
            None,
            "an unverified id proves nothing"
        );
        let long = "x".repeat(200);
        assert_eq!(
            delivery_id(
                Some(&github),
                &headers(&[("x-github-delivery", &long)]),
                b""
            ),
            None
        );
    }

    fn rules(v: serde_json::Value) -> Result<Vec<AcceptRule>, HookError> {
        request(json!({ "name": "h", "accept": v }))
            .validate()
            .map(|s| s.accept.unwrap())
    }

    #[test]
    fn checks_accept_rules() {
        let ok = rules(json!([
            { "headers": { "X-GitHub-Event": ["workflow_run"] }, "path": "/workflow_run/conclusion", "in": ["failure"] },
            { "headers": { "x-github-event": ["pull_request_review"] } },
        ]))
        .unwrap();
        assert_eq!(
            ok[0].headers.keys().collect::<Vec<_>>(),
            ["x-github-event"],
            "names lowercase"
        );
        for bad in [
            json!([]),
            json!([{}]),
            json!([{ "path": "/a" }]),
            json!([{ "path": "a", "in": ["x"] }]),
            json!([{ "path": "/a", "in": [{ "nested": 1 }] }]),
            json!([{ "headers": { "bad name": ["x"] } }]),
            json!([{ "headers": { "x-a": [] } }]),
            json!([{ "headers": { "x-a": [" push"] } }]),
            json!([{ "headers": { "x-a": ["x".repeat(257)] } }]),
            json!([{ "headers": { "X-A": ["1"], "x-a": ["2"] } }]),
            json!([{ "headers": { "a": ["1"], "b": ["1"], "c": ["1"], "d": ["1"], "e": ["1"], "f": ["1"] } }]),
            json!((0..6)
                .map(|_| json!({ "headers": { "x": ["1"] } }))
                .collect::<Vec<_>>()),
        ] {
            let err = rules(bad.clone()).unwrap_err();
            assert_eq!(err.field(), "accept", "{bad}: {err}");
        }
        assert!(serde_json::from_value::<HookRequest>(
            json!({ "name": "h", "accept": [{ "regex": "x" }] })
        )
        .is_err());
    }

    #[test]
    fn matches_any_rule_with_all_its_parts() {
        let r = rules(json!([
            { "headers": { "x-github-event": ["workflow_run"] }, "path": "/workflow_run/conclusion", "in": ["failure", "timed_out"] },
            { "headers": { "x-github-event": ["pull_request_review"] } },
        ]))
        .unwrap();
        let event = |e: &str| headers(&[("x-github-event", e)]);
        let run = |c: &str| format!(r#"{{"workflow_run":{{"conclusion":"{c}"}}}}"#);
        assert!(accepts(
            &r,
            &event("workflow_run"),
            run("failure").as_bytes()
        ));
        assert!(!accepts(
            &r,
            &event("workflow_run"),
            run("success").as_bytes()
        ));
        assert!(!accepts(&r, &event("check_run"), run("failure").as_bytes()));
        assert!(accepts(&r, &event("pull_request_review"), b"not json"));
        assert!(
            !accepts(&r, &event("workflow_run"), b"not json"),
            "no JSON, no path"
        );
        assert!(
            !accepts(&r, &[], run("failure").as_bytes()),
            "a missing header doesn't match"
        );
        assert!(
            !accepts(&r, &event("Workflow_Run"), run("failure").as_bytes()),
            "values are exact"
        );
    }

    #[test]
    fn validates_updates_and_tells_null_from_absent() {
        let patch = |v| serde_json::from_value::<HookUpdate>(v).unwrap().validate();
        let p = patch(json!({})).unwrap();
        assert_eq!(
            (
                p.verify.is_none(),
                p.accept.is_none(),
                p.keep_headers.is_none()
            ),
            (true, true, true)
        );
        let p = patch(json!({ "verify": null, "accept": null })).unwrap();
        assert_eq!((p.verify, p.accept), (Some(None), Some(None)));
        let p = patch(json!({ "accept": [{ "headers": { "X-A": ["1"] } }] })).unwrap();
        assert!(p.accept.unwrap().unwrap()[0].headers.contains_key("x-a"));
        assert_eq!(
            patch(json!({ "accept": [] })).unwrap_err().field(),
            "accept"
        );
        let p = patch(json!({ "verify": { "scheme": "github", "secret": SECRET } })).unwrap();
        assert!(!format!("{p:?}").contains(SECRET));
        assert!(serde_json::from_value::<HookUpdate>(json!({ "name": "h" })).is_err());
        let github = spec(Scheme::Github);
        assert_eq!(
            check_keep(Some(&github), vec!["X-Hub-Signature-256".into()])
                .unwrap_err()
                .field(),
            "keep_headers"
        );
    }

    #[test]
    fn cuts_on_character_boundaries() {
        assert_eq!(cut("héllo", 2), "h");
        assert_eq!(cut("abc", 10), "abc");
    }
}
