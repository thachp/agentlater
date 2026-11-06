//! Delivery signatures.
//!
//! Every outbound request carries:
//!
//! ```text
//! RunLater-Task-Id:   task_…
//! RunLater-Attempt:   1
//! RunLater-Timestamp: 1767225600            (unix seconds)
//! RunLater-Signature: v1=<hex HMAC-SHA256>
//! ```
//!
//! The MAC covers `"{timestamp}.{task_id}.{attempt}."` followed by the raw body bytes,
//! keyed with the project's signing secret. During key rotation the signature header
//! holds several comma-separated `v1=` entries; a receiver accepts if any one matches.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::fmt;

pub const HEADER_TASK_ID: &str = "RunLater-Task-Id";
pub const HEADER_ATTEMPT: &str = "RunLater-Attempt";
pub const HEADER_TIMESTAMP: &str = "RunLater-Timestamp";
pub const HEADER_SIGNATURE: &str = "RunLater-Signature";

/// How far a receiver should allow the timestamp to drift from its own clock.
pub const SIGNATURE_TOLERANCE_SECS: i64 = 300;

const SCHEME: &str = "v1=";

/// The signed headers for one delivery attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub task_id: String,
    pub attempt: u32,
    pub timestamp: i64,
    pub signature: String,
}

impl Signature {
    pub fn headers(&self) -> [(&'static str, String); 4] {
        [
            (HEADER_TASK_ID, self.task_id.clone()),
            (HEADER_ATTEMPT, self.attempt.to_string()),
            (HEADER_TIMESTAMP, self.timestamp.to_string()),
            (HEADER_SIGNATURE, self.signature.clone()),
        ]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignatureError {
    Expired,
    Malformed,
    Mismatch,
}

impl fmt::Display for SignatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Expired => "signature timestamp is outside the allowed window",
            Self::Malformed => "signature header is malformed",
            Self::Mismatch => "signature does not match",
        })
    }
}

impl std::error::Error for SignatureError {}

/// Signs one attempt with every currently active secret (normally one; two while rotating).
pub fn sign(
    secrets: &[&[u8]],
    task_id: &str,
    attempt: u32,
    timestamp: i64,
    body: &[u8],
) -> Signature {
    let signature = secrets
        .iter()
        .map(|secret| {
            let tag = mac(secret, task_id, attempt, timestamp, body).finalize();
            format!("{SCHEME}{}", hex::encode(tag.into_bytes()))
        })
        .collect::<Vec<_>>()
        .join(",");
    Signature {
        task_id: task_id.into(),
        attempt,
        timestamp,
        signature,
    }
}

/// Receiver-side check, also used by RunLater's own tests and examples.
pub fn verify(
    secret: &[u8],
    headers: &Signature,
    body: &[u8],
    now: i64,
) -> Result<(), SignatureError> {
    if headers.timestamp.abs_diff(now) > SIGNATURE_TOLERANCE_SECS as u64 {
        return Err(SignatureError::Expired);
    }
    let mut saw_v1 = false;
    for entry in headers.signature.split(',').map(str::trim) {
        let Some(hex_tag) = entry.strip_prefix(SCHEME) else {
            continue;
        };
        saw_v1 = true;
        let Ok(tag) = hex::decode(hex_tag) else {
            continue;
        };
        let m = mac(
            secret,
            &headers.task_id,
            headers.attempt,
            headers.timestamp,
            body,
        );
        if m.verify_slice(&tag).is_ok() {
            return Ok(());
        }
    }
    Err(if saw_v1 {
        SignatureError::Mismatch
    } else {
        SignatureError::Malformed
    })
}

fn mac(secret: &[u8], task_id: &str, attempt: u32, timestamp: i64, body: &[u8]) -> Hmac<Sha256> {
    let mut m = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    m.update(format!("{timestamp}.{task_id}.{attempt}.").as_bytes());
    m.update(body);
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_767_225_600;

    #[test]
    fn matches_a_fixed_vector() {
        // Same as Python: hmac.new(b"secret", b"1767225600.task_1.1.{}", sha256).hexdigest()
        let s = sign(&[b"secret"], "task_1", 1, NOW, b"{}");
        assert_eq!(
            s.signature,
            "v1=78034213583182231b9638987a3393e0a8a1a8ba1fa5810e5822c53d835a2eb2"
        );
    }

    #[test]
    fn round_trips_and_exposes_headers() {
        let s = sign(&[b"secret"], "task_1", 2, NOW, b"hello");
        assert_eq!(verify(b"secret", &s, b"hello", NOW + 10), Ok(()));
        let names: Vec<_> = s.headers().iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            [
                HEADER_TASK_ID,
                HEADER_ATTEMPT,
                HEADER_TIMESTAMP,
                HEADER_SIGNATURE
            ]
        );
        assert_eq!(s.headers()[1].1, "2");
    }

    #[test]
    fn rejects_tampering() {
        let s = sign(&[b"secret"], "task_1", 1, NOW, b"hello");
        assert_eq!(
            verify(b"other", &s, b"hello", NOW),
            Err(SignatureError::Mismatch)
        );
        assert_eq!(
            verify(b"secret", &s, b"hellO", NOW),
            Err(SignatureError::Mismatch)
        );
        let replayed = Signature {
            attempt: 2,
            ..s.clone()
        };
        assert_eq!(
            verify(b"secret", &replayed, b"hello", NOW),
            Err(SignatureError::Mismatch)
        );
        let moved = Signature {
            task_id: "task_2".into(),
            ..s
        };
        assert_eq!(
            verify(b"secret", &moved, b"hello", NOW),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn enforces_the_time_window() {
        let s = sign(&[b"secret"], "task_1", 1, NOW, b"");
        assert!(verify(b"secret", &s, b"", NOW + SIGNATURE_TOLERANCE_SECS).is_ok());
        assert_eq!(
            verify(b"secret", &s, b"", NOW + SIGNATURE_TOLERANCE_SECS + 1),
            Err(SignatureError::Expired)
        );
    }

    #[test]
    fn supports_rotation_with_multiple_signatures() {
        let s = sign(&[b"old", b"new"], "task_1", 1, NOW, b"x");
        assert_eq!(s.signature.matches(SCHEME).count(), 2);
        assert!(verify(b"old", &s, b"x", NOW).is_ok());
        assert!(verify(b"new", &s, b"x", NOW).is_ok());
    }

    #[test]
    fn rejects_unknown_schemes() {
        let s = Signature {
            task_id: "task_1".into(),
            attempt: 1,
            timestamp: NOW,
            signature: "v0=abcd".into(),
        };
        assert_eq!(
            verify(b"secret", &s, b"", NOW),
            Err(SignatureError::Malformed)
        );
    }
}
