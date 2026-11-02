use crate::limits::{
    DAY_MS, DEFAULT_MAX_ATTEMPTS, HOUR_MS, MAX_ATTEMPTS, MAX_BODY_BYTES, MAX_HEADERS,
    MAX_HEADER_BYTES, MAX_IDEMPOTENCY_KEY_BYTES, MAX_LANE_BYTES, MAX_SCHEDULE_AHEAD_MS, MINUTE_MS,
    SECOND_MS,
};
use runlater_egress::{validate_header, Destination, DestinationError, HeaderError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Method {
    Get,
    #[default]
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

/// `schedule_http` input exactly as the caller sent it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleRequest {
    pub url: String,
    #[serde(default)]
    pub method: Method,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
    /// Unix milliseconds. Absent or in the past means "as soon as possible".
    #[serde(default)]
    pub run_at_ms: Option<i64>,
    /// Relative alternative to `run_at_ms`, e.g. `"90s"`, `"15m"`, `"24h"`, `"7d"`.
    #[serde(default)]
    pub delay: Option<String>,
    /// Where to POST the task's final state once it succeeds, fails or is canceled, or
    /// [`INBOX`] to add it to the project's inbox instead.
    #[serde(default)]
    pub callback_url: Option<String>,
    /// Keep the destination's last response (status, a few headers, the start of the body).
    #[serde(default)]
    pub capture: bool,
    /// Tasks sharing a lane run one at a time, in order.
    #[serde(default)]
    pub lane: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub max_attempts: Option<u32>,
}

/// The `callback_url` (or a cron's `alert_url`) that reports to the project's inbox.
pub const INBOX: &str = "inbox";

/// Where a finished task, or a cron alert, is reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Callback {
    /// A signed POST to this destination: a callback task, with retries.
    Url(Destination),
    /// An event in the project's inbox, read with `read_inbox`.
    Inbox,
}

impl Callback {
    pub fn parse(s: &str) -> Result<Self, DestinationError> {
        if s == INBOX {
            Ok(Self::Inbox)
        } else {
            Destination::parse(s).map(Self::Url)
        }
    }

    /// As stored and reported: the normalized URL, or `"inbox"`.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Url(d) => d.as_str(),
            Self::Inbox => INBOX,
        }
    }
}

/// A validated request, ready to become a task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSpec {
    pub destination: Destination,
    pub method: Method,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub run_at_ms: i64,
    pub max_attempts: u32,
    pub idempotency_key: Option<String>,
    pub callback: Option<Callback>,
    pub capture: bool,
    pub lane: Option<String>,
    /// Hash of the request as submitted. A repeated idempotency key with a different
    /// fingerprint is a conflict, not a replay.
    pub fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValidationError {
    Destination(DestinationError),
    Header(HeaderError),
    DuplicateHeader(String),
    TooManyHeaders,
    HeadersTooLarge,
    BodyTooLarge,
    BodyNotAllowed,
    RunAtNegative,
    RunAtTooFar,
    RunAtAndDelay,
    Delay,
    DelayTooFar,
    Callback(DestinationError),
    Lane,
    MaxAttempts,
    IdempotencyKey,
}

impl ValidationError {
    /// The request field at fault, for error responses.
    pub fn field(&self) -> &'static str {
        match self {
            Self::Destination(_) => "url",
            Self::Header(_) | Self::DuplicateHeader(_) | Self::TooManyHeaders => "headers",
            Self::HeadersTooLarge => "headers",
            Self::BodyTooLarge | Self::BodyNotAllowed => "body",
            Self::RunAtNegative | Self::RunAtTooFar => "run_at_ms",
            Self::RunAtAndDelay | Self::Delay | Self::DelayTooFar => "delay",
            Self::Callback(_) => "callback_url",
            Self::Lane => "lane",
            Self::MaxAttempts => "max_attempts",
            Self::IdempotencyKey => "idempotency_key",
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Destination(e) => e.fmt(f),
            Self::Header(e) => e.fmt(f),
            Self::DuplicateHeader(n) => write!(f, "header {n:?} is given more than once"),
            Self::TooManyHeaders => write!(f, "at most {MAX_HEADERS} headers are allowed"),
            Self::HeadersTooLarge => write!(f, "headers exceed {MAX_HEADER_BYTES} bytes"),
            Self::BodyTooLarge => write!(f, "body exceeds {MAX_BODY_BYTES} bytes"),
            Self::BodyNotAllowed => f.write_str("GET requests cannot have a body"),
            Self::RunAtNegative => f.write_str("run_at_ms must not be negative"),
            Self::RunAtTooFar => f.write_str("run_at_ms is more than 7 days away"),
            Self::RunAtAndDelay => f.write_str("give run_at_ms or delay, not both"),
            Self::Delay => f.write_str(
                "delay must be a whole number followed by s, m, h or d, e.g. \"90s\" or \"24h\"",
            ),
            Self::DelayTooFar => f.write_str("delay is more than 7 days"),
            Self::Callback(e) => write!(f, "callback_url: {e}"),
            Self::Lane => write!(
                f,
                "lane must be 1-{MAX_LANE_BYTES} characters of A-Z a-z 0-9 _ - . :"
            ),
            Self::MaxAttempts => write!(f, "max_attempts must be between 1 and {MAX_ATTEMPTS}"),
            Self::IdempotencyKey => write!(
                f,
                "idempotency_key must be 1-{MAX_IDEMPOTENCY_KEY_BYTES} characters of A-Z a-z 0-9 _ -"
            ),
        }
    }
}

impl std::error::Error for ValidationError {}

impl ScheduleRequest {
    pub fn validate(self, now_ms: i64) -> Result<TaskSpec, ValidationError> {
        let destination = Destination::parse(&self.url).map_err(ValidationError::Destination)?;

        if self.headers.len() > MAX_HEADERS {
            return Err(ValidationError::TooManyHeaders);
        }
        let mut seen = BTreeSet::new();
        let mut header_bytes = 0;
        for (name, value) in &self.headers {
            validate_header(name, value).map_err(ValidationError::Header)?;
            if !seen.insert(name.to_ascii_lowercase()) {
                return Err(ValidationError::DuplicateHeader(name.clone()));
            }
            header_bytes += name.len() + value.len();
        }
        if header_bytes > MAX_HEADER_BYTES {
            return Err(ValidationError::HeadersTooLarge);
        }

        if let Some(body) = &self.body {
            if self.method == Method::Get {
                return Err(ValidationError::BodyNotAllowed);
            }
            if body.len() > MAX_BODY_BYTES {
                return Err(ValidationError::BodyTooLarge);
            }
        }

        let run_at_ms = match (self.run_at_ms, &self.delay) {
            (Some(_), Some(_)) => return Err(ValidationError::RunAtAndDelay),
            (Some(t), None) if t < 0 => return Err(ValidationError::RunAtNegative),
            (Some(t), None) if t > now_ms.saturating_add(MAX_SCHEDULE_AHEAD_MS) => {
                return Err(ValidationError::RunAtTooFar)
            }
            (Some(t), None) => t.max(now_ms),
            (None, Some(d)) => {
                let ms = parse_delay(d).ok_or(ValidationError::Delay)?;
                if ms > MAX_SCHEDULE_AHEAD_MS {
                    return Err(ValidationError::DelayTooFar);
                }
                now_ms.saturating_add(ms)
            }
            (None, None) => now_ms,
        };

        let callback = self
            .callback_url
            .as_deref()
            .map(Callback::parse)
            .transpose()
            .map_err(ValidationError::Callback)?;

        let max_attempts = self.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS);
        if !(1..=MAX_ATTEMPTS).contains(&max_attempts) {
            return Err(ValidationError::MaxAttempts);
        }

        if let Some(key) = &self.idempotency_key {
            if key.is_empty()
                || key.len() > MAX_IDEMPOTENCY_KEY_BYTES
                || !key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err(ValidationError::IdempotencyKey);
            }
        }

        if let Some(lane) = &self.lane {
            if !valid_lane(lane) {
                return Err(ValidationError::Lane);
            }
        }

        let fingerprint = self.fingerprint(&destination, callback.as_ref());
        Ok(TaskSpec {
            destination,
            method: self.method,
            headers: self.headers.into_iter().collect(),
            body: self.body,
            run_at_ms,
            max_attempts,
            idempotency_key: self.idempotency_key,
            callback,
            capture: self.capture,
            lane: self.lane,
            fingerprint,
        })
    }

    /// Uses the normalized URL and lowercased header names so cosmetic differences replay,
    /// and the *submitted* `run_at_ms` or `delay` so "now" resolving differently on a retry
    /// still matches. Fields added after v1 are only hashed when present, so fingerprints of
    /// requests that do not use them are unchanged.
    fn fingerprint(&self, destination: &Destination, callback: Option<&Callback>) -> String {
        let headers: BTreeMap<_, _> = self
            .headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect();
        let canonical = serde_json::to_vec(&(
            "schedule_http/v1",
            destination.as_str(),
            self.method,
            headers,
            &self.body,
            self.run_at_ms,
            self.max_attempts,
        ))
        .expect("plain data always serializes");
        let mut hash = Sha256::new();
        hash.update(canonical);
        // Each later field adds its own tagged entry, so adding another field never changes
        // the fingerprint of a request that does not use it.
        let extras = [
            ("delay", self.delay.as_deref()),
            ("callback_url", callback.map(Callback::as_str)),
            ("lane", self.lane.as_deref()),
            ("capture", self.capture.then_some("true")),
        ];
        for (name, value) in extras {
            if let Some(value) = value {
                hash.update(
                    serde_json::to_vec(&(name, value)).expect("plain data always serializes"),
                );
            }
        }
        hex::encode(hash.finalize())
    }
}

/// Lane names: what `schedule_http` accepts and `list_tasks` filters on.
pub fn valid_lane(lane: &str) -> bool {
    !lane.is_empty()
        && lane.len() <= MAX_LANE_BYTES
        && lane
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
}

/// `"<n><unit>"` with unit `s`, `m`, `h` or `d`, in milliseconds.
pub(crate) fn parse_delay(s: &str) -> Option<i64> {
    let unit = match s.as_bytes().last()? {
        b's' => SECOND_MS,
        b'm' => MINUTE_MS,
        b'h' => HOUR_MS,
        b'd' => DAY_MS,
        _ => return None,
    };
    let digits = &s[..s.len() - 1];
    if digits.is_empty() || digits.len() > 7 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<i64>().ok()?.checked_mul(unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_767_225_600_000;

    fn req(url: &str) -> ScheduleRequest {
        ScheduleRequest {
            url: url.into(),
            ..Default::default()
        }
    }

    fn ok() -> ScheduleRequest {
        req("https://hooks.example.com/run")
    }

    fn invalid(r: ScheduleRequest) -> ValidationError {
        r.validate(NOW).unwrap_err()
    }

    #[test]
    fn minimal_request_gets_defaults() {
        let spec = ok().validate(NOW).unwrap();
        assert_eq!(spec.method, Method::Post);
        assert_eq!(spec.run_at_ms, NOW);
        assert_eq!(spec.max_attempts, DEFAULT_MAX_ATTEMPTS);
        assert_eq!(spec.destination.as_str(), "https://hooks.example.com/run");
        assert_eq!(spec.fingerprint.len(), 64);
    }

    #[test]
    fn parses_json_and_rejects_unknown_fields() {
        let r: ScheduleRequest = serde_json::from_str(
            r#"{"url":"https://a.example.com/","method":"PUT","headers":{"X-A":"1"},"body":"{}","run_at_ms":5}"#,
        )
        .unwrap();
        assert_eq!(r.method, Method::Put);
        assert!(serde_json::from_str::<ScheduleRequest>(
            r#"{"url":"https://a.example.com/","callback":"x"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<ScheduleRequest>(
            r#"{"url":"https://a.example.com/","method":"TRACE"}"#
        )
        .is_err());
    }

    #[test]
    fn rejects_unsafe_destinations() {
        assert_eq!(
            invalid(req("http://example.com/")),
            ValidationError::Destination(DestinationError::NotHttps)
        );
        assert_eq!(invalid(req("https://10.0.0.1/")).field(), "url");
    }

    #[test]
    fn checks_headers() {
        let mut r = ok();
        r.headers.insert("Host".into(), "evil.example.com".into());
        assert!(matches!(invalid(r), ValidationError::Header(_)));

        let mut r = ok();
        r.headers.insert("X-Token".into(), "a".into());
        r.headers.insert("x-token".into(), "b".into());
        assert_eq!(
            invalid(r),
            ValidationError::DuplicateHeader("x-token".into())
        );

        let mut r = ok();
        r.headers = (0..=MAX_HEADERS)
            .map(|i| (format!("X-{i}"), "v".into()))
            .collect();
        assert_eq!(invalid(r), ValidationError::TooManyHeaders);

        let mut r = ok();
        r.headers
            .insert("X-Big".into(), "v".repeat(MAX_HEADER_BYTES));
        assert_eq!(invalid(r), ValidationError::HeadersTooLarge);
    }

    #[test]
    fn checks_body() {
        let mut r = ok();
        r.body = Some("x".repeat(MAX_BODY_BYTES));
        assert!(r.validate(NOW).is_ok());

        let mut r = ok();
        r.body = Some("x".repeat(MAX_BODY_BYTES + 1));
        assert_eq!(invalid(r), ValidationError::BodyTooLarge);

        let mut r = ok();
        r.method = Method::Get;
        r.body = Some(String::new());
        assert_eq!(invalid(r), ValidationError::BodyNotAllowed);
    }

    #[test]
    fn checks_run_at() {
        let at = |t| ScheduleRequest {
            run_at_ms: Some(t),
            ..ok()
        };
        assert_eq!(at(NOW - 1000).validate(NOW).unwrap().run_at_ms, NOW);
        assert_eq!(at(NOW + 5000).validate(NOW).unwrap().run_at_ms, NOW + 5000);
        assert!(at(NOW + 7 * DAY_MS).validate(NOW).is_ok());
        assert_eq!(
            invalid(at(NOW + 7 * DAY_MS + 1)),
            ValidationError::RunAtTooFar
        );
        assert_eq!(invalid(at(-1)), ValidationError::RunAtNegative);
    }

    #[test]
    fn checks_max_attempts_and_idempotency_key() {
        for n in [0, MAX_ATTEMPTS + 1] {
            let r = ScheduleRequest {
                max_attempts: Some(n),
                ..ok()
            };
            assert_eq!(invalid(r), ValidationError::MaxAttempts);
        }
        for key in [
            "",
            "has space",
            "ünicode",
            &"k".repeat(MAX_IDEMPOTENCY_KEY_BYTES + 1),
        ] {
            let r = ScheduleRequest {
                idempotency_key: Some(key.into()),
                ..ok()
            };
            assert_eq!(invalid(r), ValidationError::IdempotencyKey, "{key:?}");
        }
    }

    #[test]
    fn fingerprint_ignores_cosmetics_but_not_content() {
        let fp = |r: ScheduleRequest, now| r.validate(now).unwrap().fingerprint;
        let mut a = req("https://HOOKS.example.com/run");
        a.headers.insert("X-Token".into(), "1".into());
        let mut b = ok();
        b.headers.insert("x-token".into(), "1".into());
        assert_eq!(fp(a.clone(), NOW), fp(b, NOW + 60_000));

        let mut c = a.clone();
        c.body = Some("{}".into());
        assert_ne!(fp(a.clone(), NOW), fp(c, NOW));
        let d = ScheduleRequest {
            method: Method::Put,
            ..a.clone()
        };
        assert_ne!(fp(a, NOW), fp(d, NOW));
    }

    #[test]
    fn delay_is_relative_to_now() {
        let after = |d: &str| ScheduleRequest {
            delay: Some(d.into()),
            ..ok()
        };
        let at = |d: &str| after(d).validate(NOW).unwrap().run_at_ms;
        assert_eq!(at("0s"), NOW);
        assert_eq!(at("90s"), NOW + 90 * SECOND_MS);
        assert_eq!(at("15m"), NOW + 15 * MINUTE_MS);
        assert_eq!(at("24h"), NOW + DAY_MS);
        assert_eq!(at("7d"), NOW + 7 * DAY_MS);
        assert_eq!(invalid(after("8d")), ValidationError::DelayTooFar);
        assert_eq!(invalid(after("9999999d")), ValidationError::DelayTooFar);
        for bad in [
            "",
            "h",
            "24",
            "1.5h",
            "-1h",
            "1H",
            "1w",
            "1h30m",
            " 1h",
            "+1h",
            "99999999s",
        ] {
            assert_eq!(invalid(after(bad)), ValidationError::Delay, "{bad:?}");
        }
        let both = ScheduleRequest {
            run_at_ms: Some(NOW),
            ..after("1h")
        };
        assert_eq!(invalid(both).field(), "delay");
    }

    #[test]
    fn checks_callback_url() {
        let cb = |u: &str| ScheduleRequest {
            callback_url: Some(u.into()),
            ..ok()
        };
        let spec = cb("https://HOOKS.example.com/done").validate(NOW).unwrap();
        assert_eq!(
            spec.callback.unwrap().as_str(),
            "https://hooks.example.com/done"
        );
        assert_eq!(ok().validate(NOW).unwrap().callback, None);
        assert_eq!(
            cb("inbox").validate(NOW).unwrap().callback,
            Some(Callback::Inbox)
        );
        assert!(matches!(invalid(cb("INBOX")), ValidationError::Callback(_)));
        let e = invalid(cb("https://localhost/done"));
        assert!(matches!(e, ValidationError::Callback(_)));
        assert_eq!(e.field(), "callback_url");
    }

    #[test]
    fn fingerprint_covers_delay_and_callback_without_changing_v1() {
        let fp = |r: ScheduleRequest, now| r.validate(now).unwrap().fingerprint;
        // Pinned: v1 fingerprints must not change, or stored idempotency keys would conflict.
        assert_eq!(
            fp(ok(), NOW),
            "429b2ce0cefc16b752fe2b3c5e25be90d9fa34029d39f6c41d473be42d83c689"
        );
        let delayed = ScheduleRequest {
            delay: Some("1h".into()),
            ..ok()
        };
        assert_eq!(fp(delayed.clone(), NOW), fp(delayed.clone(), NOW + 60_000));
        assert_ne!(fp(delayed, NOW), fp(ok(), NOW));
        let called = ScheduleRequest {
            callback_url: Some("https://hooks.example.com/done".into()),
            ..ok()
        };
        assert_ne!(fp(called.clone(), NOW), fp(ok(), NOW));
        let shouty = ScheduleRequest {
            callback_url: Some("https://HOOKS.example.com/done".into()),
            ..ok()
        };
        assert_eq!(fp(shouty, NOW), fp(called, NOW));
    }

    #[test]
    fn capture_is_off_by_default_and_part_of_the_fingerprint() {
        assert!(!ok().validate(NOW).unwrap().capture);
        let captured = ScheduleRequest {
            capture: true,
            ..ok()
        };
        let spec = captured.clone().validate(NOW).unwrap();
        assert!(spec.capture);
        assert_ne!(spec.fingerprint, ok().validate(NOW).unwrap().fingerprint);
        let inbox = ScheduleRequest {
            callback_url: Some(INBOX.into()),
            ..ok()
        };
        assert_ne!(
            inbox.validate(NOW).unwrap().fingerprint,
            ok().validate(NOW).unwrap().fingerprint
        );
    }

    #[test]
    fn checks_lane() {
        let lane = |l: &str| ScheduleRequest {
            lane: Some(l.into()),
            ..ok()
        };
        let spec = lane("payments:user-42.eu_1").validate(NOW).unwrap();
        assert_eq!(spec.lane.as_deref(), Some("payments:user-42.eu_1"));
        for bad in [
            "",
            "has space",
            "a/b",
            "ünicode",
            &"l".repeat(MAX_LANE_BYTES + 1),
        ] {
            assert_eq!(invalid(lane(bad)), ValidationError::Lane, "{bad:?}");
        }
        assert!(lane(&"l".repeat(MAX_LANE_BYTES)).validate(NOW).is_ok());
        let fp = |r: ScheduleRequest| r.validate(NOW).unwrap().fingerprint;
        assert_ne!(fp(lane("a")), fp(lane("b")));
        assert_ne!(fp(lane("a")), fp(ok()));
    }
}
