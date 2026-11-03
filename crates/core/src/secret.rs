//! Project secrets: values a request refers to as `{{secret:NAME}}` in a header value, filled
//! in only when the request is sent, and only toward the hosts the secret allows.
//!
//! Nothing here sees a stored secret: the API seals values before the ledger stores them.
//! This module checks what callers send, matches hosts, and finds and fills references.

use crate::limits::{MAX_ALLOWED_HOSTS, MAX_SECRET_BYTES, MAX_SECRET_NAME_BYTES};
use runlater_egress::Destination;
use serde::{Deserialize, Serialize};
use std::fmt;

const OPEN: &str = "{{secret:";
const CLOSE: &str = "}}";

/// `set_secret` input exactly as the caller sent it.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRequest {
    pub name: String,
    pub value: String,
    /// Hosts the secret may be sent to: `api.example.com`, or `*.example.com` for any
    /// subdomain (not `example.com` itself).
    pub allowed_hosts: Vec<String>,
}

/// Never prints the value.
impl fmt::Debug for SecretRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretRequest")
            .field("name", &self.name)
            .field("allowed_hosts", &self.allowed_hosts)
            .finish_non_exhaustive()
    }
}

/// A validated secret: hosts lowercased, sorted and deduplicated.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretSpec {
    pub name: String,
    pub value: String,
    pub allowed_hosts: Vec<String>,
}

impl fmt::Debug for SecretSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretSpec")
            .field("name", &self.name)
            .field("allowed_hosts", &self.allowed_hosts)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum SecretError {
    Name,
    Value,
    ValueTooLarge,
    NoHosts,
    TooManyHosts,
    Host(String),
}

impl SecretError {
    pub fn field(&self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Value | Self::ValueTooLarge => "value",
            Self::NoHosts | Self::TooManyHosts | Self::Host(_) => "allowed_hosts",
        }
    }
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name => write!(
                f,
                "name must be 1-{MAX_SECRET_NAME_BYTES} characters of A-Z 0-9 _, starting with a letter"
            ),
            Self::Value => f.write_str(
                "value must be printable ASCII (it is sent in a header), with no line breaks",
            ),
            Self::ValueTooLarge => write!(f, "value exceeds {MAX_SECRET_BYTES} bytes"),
            Self::NoHosts => f.write_str("allowed_hosts must name at least one host"),
            Self::TooManyHosts => write!(f, "allowed_hosts may name at most {MAX_ALLOWED_HOSTS} hosts"),
            Self::Host(h) => write!(
                f,
                "{h:?} is not a public host name or *.domain pattern (no scheme, port or path)"
            ),
        }
    }
}

impl std::error::Error for SecretError {}

pub fn valid_secret_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SECRET_NAME_BYTES
        && name.as_bytes()[0].is_ascii_uppercase()
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// A host name on its own, as `Destination` would accept it in a URL.
fn public_host(host: &str) -> Option<String> {
    let lower = host.to_ascii_lowercase();
    if lower.is_empty()
        || !lower
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return None;
    }
    let d = Destination::parse(&format!("https://{lower}/")).ok()?;
    (d.host() == lower).then_some(lower)
}

impl SecretRequest {
    pub fn validate(self) -> Result<SecretSpec, SecretError> {
        if !valid_secret_name(&self.name) {
            return Err(SecretError::Name);
        }
        if self.value.len() > MAX_SECRET_BYTES {
            return Err(SecretError::ValueTooLarge);
        }
        // Header-safe: visible ASCII, spaces and tabs. A line break would split the header.
        if self.value.is_empty()
            || !self
                .value
                .bytes()
                .all(|b| b == b'\t' || (0x20..0x7f).contains(&b))
        {
            return Err(SecretError::Value);
        }
        if self.allowed_hosts.is_empty() {
            return Err(SecretError::NoHosts);
        }
        if self.allowed_hosts.len() > MAX_ALLOWED_HOSTS {
            return Err(SecretError::TooManyHosts);
        }
        let mut hosts = Vec::new();
        for raw in &self.allowed_hosts {
            let host = match raw.strip_prefix("*.") {
                // A wildcard needs a registrable-looking base: "*.com" would allow everything.
                Some(base) if base.contains('.') => public_host(base).map(|b| format!("*.{b}")),
                Some(_) => None,
                None => public_host(raw),
            }
            .ok_or_else(|| SecretError::Host(raw.clone()))?;
            hosts.push(host);
        }
        hosts.sort();
        hosts.dedup();
        Ok(SecretSpec {
            name: self.name,
            value: self.value,
            allowed_hosts: hosts,
        })
    }
}

/// Whether `host` (lowercase, as `Destination::host` gives it) matches one of the patterns.
pub fn host_allowed(patterns: &[String], host: &str) -> bool {
    patterns.iter().any(|p| match p.strip_prefix("*.") {
        Some(base) => host.len() > base.len() + 1 && host.ends_with(&format!(".{base}")),
        None => p == host,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferenceError(pub String);

impl fmt::Display for ReferenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The secret names `text` refers to, in order of first use. A `{{secret:` that is not a
/// well-formed reference is an error rather than text sent as is.
pub fn references(text: &str) -> Result<Vec<String>, ReferenceError> {
    let mut names: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        let after = &rest[start + OPEN.len()..];
        let end = after.find(CLOSE).ok_or_else(|| {
            ReferenceError("a {{secret: reference is missing its closing }}".into())
        })?;
        let name = &after[..end];
        if !valid_secret_name(name) {
            return Err(ReferenceError(format!(
                "{{{{secret:{name}}}}} does not name a secret (A-Z 0-9 _)"
            )));
        }
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
        rest = &after[end + CLOSE.len()..];
    }
    Ok(names)
}

/// The names referred to across header values.
pub fn header_references(headers: &[(String, String)]) -> Result<Vec<String>, ReferenceError> {
    let mut names: Vec<String> = Vec::new();
    for (_, value) in headers {
        for name in references(value)? {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    Ok(names)
}

/// `text` with every reference replaced by its value. None if a name has no value.
pub fn fill<'v>(text: &str, value: impl Fn(&str) -> Option<&'v str>) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let after = &rest[start + OPEN.len()..];
        let end = after.find(CLOSE)?;
        out.push_str(value(&after[..end])?);
        rest = &after[end + CLOSE.len()..];
    }
    out.push_str(rest);
    Some(out)
}

/// `text` with each secret's value replaced by `[secret:NAME]`, for responses that echo a
/// credential back. Longer values first, so one that contains another is caught whole.
pub fn redact(text: &str, secrets: &[(String, String)]) -> String {
    let mut sorted: Vec<&(String, String)> =
        secrets.iter().filter(|(_, v)| !v.is_empty()).collect();
    sorted.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    let mut out = text.to_string();
    for (name, value) in sorted {
        out = out.replace(value.as_str(), &format!("[secret:{name}]"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(hosts: &[&str]) -> SecretRequest {
        SecretRequest {
            name: "GH_TOKEN".into(),
            value: "ghp_abc123".into(),
            allowed_hosts: hosts.iter().map(|h| h.to_string()).collect(),
        }
    }

    #[test]
    fn validates_names_values_and_hosts() {
        let spec = secret(&["API.github.com", "*.Example.com", "api.github.com"])
            .validate()
            .unwrap();
        assert_eq!(spec.allowed_hosts, ["*.example.com", "api.github.com"]);
        for name in [
            "",
            "gh",
            "1GH",
            "GH-TOKEN",
            &"G".repeat(MAX_SECRET_NAME_BYTES + 1),
        ] {
            let r = SecretRequest {
                name: name.into(),
                ..secret(&["a.example.com"])
            };
            assert_eq!(r.validate().unwrap_err(), SecretError::Name, "{name:?}");
        }
        for value in ["", "line\nbreak", "cr\r", "ünicode"] {
            let r = SecretRequest {
                value: value.into(),
                ..secret(&["a.example.com"])
            };
            assert_eq!(r.validate().unwrap_err().field(), "value", "{value:?}");
        }
        let big = SecretRequest {
            value: "x".repeat(MAX_SECRET_BYTES + 1),
            ..secret(&["a.example.com"])
        };
        assert_eq!(big.validate().unwrap_err(), SecretError::ValueTooLarge);
        assert_eq!(secret(&[]).validate().unwrap_err(), SecretError::NoHosts);
        for bad in [
            "https://api.github.com",
            "api.github.com:8443",
            "api.github.com/x",
            "localhost",
            "10.0.0.1",
            "*.com",
            "*",
            "a.*.example.com",
            "svc.internal",
        ] {
            assert!(
                matches!(secret(&[bad]).validate(), Err(SecretError::Host(_))),
                "{bad:?}"
            );
        }
        let many: Vec<String> = (0..=MAX_ALLOWED_HOSTS)
            .map(|i| format!("h{i}.example.com"))
            .collect();
        let r = SecretRequest {
            allowed_hosts: many,
            ..secret(&[])
        };
        assert_eq!(r.validate().unwrap_err(), SecretError::TooManyHosts);
    }

    #[test]
    fn debug_never_shows_the_value() {
        let shown = format!(
            "{:?} {:?}",
            secret(&["a.example.com"]),
            secret(&["a.example.com"]).validate().unwrap()
        );
        assert!(!shown.contains("ghp_abc123"), "{shown}");
    }

    #[test]
    fn matches_hosts_exactly_or_by_subdomain() {
        let p = vec!["api.github.com".to_string(), "*.example.com".to_string()];
        assert!(host_allowed(&p, "api.github.com"));
        assert!(host_allowed(&p, "a.example.com"));
        assert!(host_allowed(&p, "a.b.example.com"));
        for no in [
            "github.com",
            "evil.api.github.com",
            "example.com",
            "badexample.com",
            "example.com.evil.net",
        ] {
            assert!(!host_allowed(&p, no), "{no}");
        }
    }

    #[test]
    fn finds_fills_and_redacts_references() {
        assert_eq!(
            references("Bearer {{secret:GH}} and {{secret:B_2}} {{secret:GH}}").unwrap(),
            ["GH", "B_2"]
        );
        assert_eq!(
            references("plain {{ text }}").unwrap(),
            Vec::<String>::new()
        );
        assert!(references("{{secret:GH").is_err());
        assert!(references("{{secret:gh}}").is_err());
        assert!(references("{{secret:}}").is_err());

        let value = |n: &str| (n == "GH").then_some("ghp_1");
        assert_eq!(
            fill("Bearer {{secret:GH}}!", value).as_deref(),
            Some("Bearer ghp_1!")
        );
        assert_eq!(fill("no refs", value).as_deref(), Some("no refs"));
        assert_eq!(fill("{{secret:OTHER}}", value), None);

        let secrets = vec![
            ("A".to_string(), "tok".to_string()),
            ("B".to_string(), "tok-long".to_string()),
        ];
        assert_eq!(
            redact(r#"{"t":"tok-long","u":"tok"}"#, &secrets),
            r#"{"t":"[secret:B]","u":"[secret:A]"}"#
        );
    }
}
