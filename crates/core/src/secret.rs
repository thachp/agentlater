//! Project secrets: values a request refers to as `{{secret:NAME}}` in a header value, filled
//! in only when the request is sent, and only toward the hosts the secret allows.
//!
//! Nothing here sees a stored secret: the API seals values before the ledger stores them.
//! This module checks what callers send, matches hosts, and finds and fills references.

use crate::limits::{MAX_ALLOWED_HOSTS, MAX_SECRET_BYTES, MAX_SECRET_NAME_BYTES, MIN_SECRET_BYTES};
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
    /// The exact host names the secret may be sent to, e.g. `api.example.com`.
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
    ValueTooShort,
    ValueTooLarge,
    NoHosts,
    TooManyHosts,
    Host(String),
}

impl SecretError {
    pub fn field(&self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Value | Self::ValueTooShort | Self::ValueTooLarge => "value",
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
            Self::ValueTooShort => write!(
                f,
                "value must be at least {MIN_SECRET_BYTES} characters; a short secret can be guessed"
            ),
            Self::ValueTooLarge => write!(f, "value exceeds {MAX_SECRET_BYTES} bytes"),
            Self::NoHosts => f.write_str("allowed_hosts must name at least one host"),
            Self::TooManyHosts => write!(f, "allowed_hosts may name at most {MAX_ALLOWED_HOSTS} hosts"),
            Self::Host(h) => write!(
                f,
                "{h:?} is not a public host name: give exact names such as api.example.com \
                 (no scheme, port, path or wildcard)"
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
        if self.value.len() < MIN_SECRET_BYTES {
            return Err(SecretError::ValueTooShort);
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
            // Exact names only. A wildcard can't be made safe: under shared domains such as
            // amazonaws.com or workers.dev anyone can get a name, and no list says which they are.
            let host = public_host(raw).ok_or_else(|| SecretError::Host(raw.clone()))?;
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

/// Whether `host` (lowercase, as `Destination::host` gives it) is one of the allowed names.
pub fn host_allowed(allowed: &[String], host: &str) -> bool {
    allowed.iter().any(|a| a == host)
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
/// credential back: as sent, JSON-escaped, or percent-encoded. Best effort: a destination that
/// transforms the value some other way (base64, masking) is not caught. Longer forms first, so
/// one value that contains another is caught whole.
pub fn redact(text: &str, secrets: &[(String, String)]) -> String {
    let mut forms: Vec<(String, &str)> = Vec::new();
    for (name, value) in secrets.iter().filter(|(_, v)| !v.is_empty()) {
        let json = value.replace('\\', r"\\").replace('"', r#"\""#);
        let json_slash = json.replace('/', r"\/");
        for form in [value.clone(), json, json_slash, percent_encode(value)] {
            if !forms.iter().any(|(f, _)| *f == form) {
                forms.push((form, name.as_str()));
            }
        }
    }
    forms.sort_by_key(|(f, _)| std::cmp::Reverse(f.len()));
    let mut out = text.to_string();
    for (form, name) in forms {
        out = out.replace(&form, &format!("[secret:{name}]"));
    }
    out
}

/// Every byte but RFC 3986's unreserved characters as `%XX`.
fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(hosts: &[&str]) -> SecretRequest {
        SecretRequest {
            name: "GH_TOKEN".into(),
            value: "ghp_abc123abc123abc".into(),
            allowed_hosts: hosts.iter().map(|h| h.to_string()).collect(),
        }
    }

    #[test]
    fn validates_names_values_and_hosts() {
        let spec = secret(&["API.github.com", "uploads.github.com", "api.github.com"])
            .validate()
            .unwrap();
        assert_eq!(spec.allowed_hosts, ["api.github.com", "uploads.github.com"]);
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
        for value in [
            "",
            "short",
            "line\nbreak-padded-out",
            "cr\r-padded-out-to-16",
            "ünicode-padded-out-16",
        ] {
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
            "*.workers.dev",
            "*.github.io",
            "*.co.uk",
            "*.amazonaws.com",
            "*.pages.dev",
            "*.example.com",
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
    fn redacts_echoes_as_sent_escaped_or_encoded() {
        let secrets = vec![("T".to_string(), r#"ab"c\d/e f+g=="#.to_string())];
        assert_eq!(redact(r#"ab"c\d/e f+g=="#, &secrets), "[secret:T]");
        assert_eq!(
            redact(r#"{"x":"ab\"c\\d/e f+g=="}"#, &secrets),
            r#"{"x":"[secret:T]"}"#
        );
        assert_eq!(redact(r#"ab\"c\\d\/e f+g=="#, &secrets), "[secret:T]");
        assert_eq!(
            redact("next=ab%22c%5Cd%2Fe%20f%2Bg%3D%3D", &secrets),
            "next=[secret:T]"
        );
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
    fn matches_hosts_exactly() {
        let p = vec!["api.github.com".to_string(), "foo.workers.dev".to_string()];
        assert!(host_allowed(&p, "api.github.com"));
        assert!(host_allowed(&p, "foo.workers.dev"));
        for no in [
            "github.com",
            "evil.api.github.com",
            "bar.workers.dev",
            "api.github.com.evil.net",
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
