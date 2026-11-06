use std::fmt;

/// Headers the platform or AgentLater controls. A caller setting these could smuggle
/// requests, spoof provenance, or forge a AgentLater signature.
const RESERVED: &[&str] = &[
    "connection",
    "content-length",
    "expect",
    "host",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "forwarded",
];
const RESERVED_PREFIXES: &[&str] = &["runlater-", "cf-", "x-forwarded-", "proxy-"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeaderError {
    InvalidName(String),
    InvalidValue(String),
    Reserved(String),
}

impl fmt::Display for HeaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(n) => write!(f, "header name {n:?} is not a valid HTTP token"),
            Self::InvalidValue(n) => write!(f, "header {n:?} has a value with control characters"),
            Self::Reserved(n) => write!(f, "header {n:?} is reserved and cannot be set"),
        }
    }
}

impl std::error::Error for HeaderError {}

/// Checks one caller-supplied outbound header.
pub fn validate_header(name: &str, value: &str) -> Result<(), HeaderError> {
    if name.is_empty() || !name.bytes().all(is_token_byte) {
        return Err(HeaderError::InvalidName(name.into()));
    }
    let lower = name.to_ascii_lowercase();
    if RESERVED.contains(&lower.as_str()) || RESERVED_PREFIXES.iter().any(|p| lower.starts_with(p))
    {
        return Err(HeaderError::Reserved(name.into()));
    }
    if value.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f) {
        return Err(HeaderError::InvalidValue(name.into()));
    }
    Ok(())
}

/// RFC 9110 `tchar`.
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_headers() {
        assert!(validate_header("Authorization", "Bearer abc").is_ok());
        assert!(validate_header("Content-Type", "application/json").is_ok());
        assert!(validate_header("X-Custom_1", "a\tb").is_ok());
    }

    #[test]
    fn rejects_reserved_headers_case_insensitively() {
        for name in [
            "Host",
            "content-length",
            "Transfer-Encoding",
            "RunLater-Signature",
            "CF-Connecting-IP",
            "X-Forwarded-For",
            "Proxy-Authorization",
        ] {
            assert_eq!(
                validate_header(name, "x"),
                Err(HeaderError::Reserved(name.into())),
                "{name}"
            );
        }
    }

    #[test]
    fn rejects_bad_names_and_injected_values() {
        assert!(matches!(
            validate_header("", "x"),
            Err(HeaderError::InvalidName(_))
        ));
        assert!(matches!(
            validate_header("Bad Name", "x"),
            Err(HeaderError::InvalidName(_))
        ));
        assert!(matches!(
            validate_header("X-A:", "x"),
            Err(HeaderError::InvalidName(_))
        ));
        assert!(matches!(
            validate_header("X-A", "ok\r\nHost: evil"),
            Err(HeaderError::InvalidValue(_))
        ));
    }
}
