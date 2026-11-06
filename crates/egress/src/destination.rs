use std::fmt;
use url::{Host, Url};

pub const MAX_URL_BYTES: usize = 2048;

/// Hostname suffixes that never name a public internet host.
const NON_PUBLIC_SUFFIXES: &[&str] = &[
    "localhost",
    "local",
    "localdomain",
    "internal",
    "intranet",
    "lan",
    "home",
    "corp",
    "onion",
    "arpa",
    "invalid",
    "test",
    "example",
];

/// A URL AgentLater is willing to deliver to: `https://` on port 443 to a public DNS name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Destination(Url);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestinationError {
    TooLong,
    Malformed,
    NotHttps,
    Credentials,
    Port,
    Fragment,
    IpLiteral,
    NotPublicHost,
}

impl fmt::Display for DestinationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLong => "URL is longer than 2048 bytes",
            Self::Malformed => "URL is not a valid absolute URL",
            Self::NotHttps => "URL must use https://",
            Self::Credentials => "URL must not contain a username or password",
            Self::Port => "URL must use the default HTTPS port",
            Self::Fragment => "URL must not contain a #fragment",
            Self::IpLiteral => "URL must use a DNS name, not an IP address",
            Self::NotPublicHost => "URL host is not a public internet name",
        })
    }
}

impl std::error::Error for DestinationError {}

impl Destination {
    pub fn parse(raw: &str) -> Result<Self, DestinationError> {
        if raw.len() > MAX_URL_BYTES {
            return Err(DestinationError::TooLong);
        }
        // Reject anything the URL parser would silently repair: whitespace, controls,
        // backslashes, and percent-encoded control bytes.
        if !raw.bytes().all(|b| (0x21..=0x7e).contains(&b)) || raw.contains('\\') {
            return Err(DestinationError::Malformed);
        }
        if has_encoded_control(raw) {
            return Err(DestinationError::Malformed);
        }
        let url = Url::parse(raw).map_err(|_| DestinationError::Malformed)?;
        if url.scheme() != "https" {
            return Err(DestinationError::NotHttps);
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(DestinationError::Credentials);
        }
        // `Url::port` is `None` for the scheme default, so an explicit `:443` is accepted.
        if url.port().is_some() {
            return Err(DestinationError::Port);
        }
        if url.fragment().is_some() {
            return Err(DestinationError::Fragment);
        }
        match url.host() {
            Some(Host::Domain(host)) if is_public_name(host) => Ok(Self(url)),
            Some(Host::Domain(_)) => Err(DestinationError::NotPublicHost),
            Some(Host::Ipv4(_) | Host::Ipv6(_)) => Err(DestinationError::IpLiteral),
            None => Err(DestinationError::Malformed),
        }
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn host(&self) -> &str {
        self.0.host_str().unwrap_or_default()
    }
}

impl fmt::Display for Destination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn has_encoded_control(raw: &str) -> bool {
    raw.as_bytes().windows(3).any(|w| {
        w[0] == b'%'
            && std::str::from_utf8(&w[1..])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                .is_some_and(|b| b < 0x20 || b == 0x7f)
    })
}

/// `url` has already lowercased the host and converted IDNs to punycode.
fn is_public_name(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host);
    host.len() <= 253
        && host.contains('.')
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        && !NON_PUBLIC_SUFFIXES
            .iter()
            .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(raw: &str) -> DestinationError {
        Destination::parse(raw).unwrap_err()
    }

    #[test]
    fn accepts_public_https_and_normalizes() {
        let d = Destination::parse("https://API.Example.com").unwrap();
        assert_eq!(d.as_str(), "https://api.example.com/");
        assert_eq!(d.host(), "api.example.com");
        assert!(Destination::parse("https://hooks.example.com:443/a?b=c").is_ok());
        assert!(Destination::parse("https://xn--bcher-kva.example.org/").is_ok());
    }

    #[test]
    fn rejects_non_https_credentials_ports_and_fragments() {
        assert_eq!(err("http://example.com/"), DestinationError::NotHttps);
        assert_eq!(err("ftp://example.com/"), DestinationError::NotHttps);
        assert_eq!(
            err("https://u:p@example.com/"),
            DestinationError::Credentials
        );
        assert_eq!(err("https://example.com:8443/"), DestinationError::Port);
        assert_eq!(err("https://example.com/#x"), DestinationError::Fragment);
    }

    #[test]
    fn rejects_ip_literals_in_every_spelling() {
        for raw in [
            "https://127.0.0.1/",
            "https://10.0.0.1/",
            "https://[::1]/",
            "https://0x7f.1/",
            "https://2130706433/",
            "https://169.254.169.254/latest",
        ] {
            assert_eq!(err(raw), DestinationError::IpLiteral, "{raw}");
        }
    }

    #[test]
    fn rejects_non_public_names() {
        for raw in [
            "https://localhost/",
            "https://intranet/",
            "https://printer.local/",
            "https://db.internal/",
            "https://api.test/",
            "https://1.0.0.127.in-addr.arpa/",
            "https://hidden.onion/",
            "https://-bad.example.com/",
        ] {
            assert_eq!(err(raw), DestinationError::NotPublicHost, "{raw}");
        }
    }

    #[test]
    fn rejects_input_the_parser_would_repair() {
        assert_eq!(err("https://example.com/a b"), DestinationError::Malformed);
        assert_eq!(err("https:\\\\example.com"), DestinationError::Malformed);
        assert_eq!(
            err("https://example.com/%0d%0a"),
            DestinationError::Malformed
        );
        assert_eq!(err("not a url"), DestinationError::Malformed);
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES));
        assert_eq!(err(&long), DestinationError::TooLong);
    }
}
