//! `set_wake`: the request RunLater sends to start the agent when its inbox has something new.
//!
//! A wake is a signed `POST` like any delivery. It carries only how many events are unread;
//! the agent reads them through the authenticated inbox. Wakes are debounced, so a burst of
//! finished tasks starts the agent once.

use crate::limits::{DEFAULT_WAKE_DEBOUNCE, MAX_WAKE_DEBOUNCE_MS, MINUTE_MS, MIN_WAKE_DEBOUNCE_MS};
use crate::request::{parse_delay, Method, ScheduleRequest, TaskSpec, ValidationError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// `set_wake` input exactly as the caller sent it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WakeRequest {
    /// What starts an agent run: a routine or workflow trigger, a chat webhook, a server.
    pub url: String,
    /// Sent on every wake, e.g. the trigger's `Authorization`. Same rules as `schedule_http`.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// At most one wake per this long, e.g. `"15m"`. Events in between share the next wake.
    #[serde(default)]
    pub debounce: Option<String>,
}

/// A validated wake, ready to store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WakeSpec {
    /// The request each wake makes. Its body is set when the wake is sent.
    pub request: TaskSpec,
    pub debounce_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WakeRequestError {
    Debounce,
    DebounceRange,
    Request(ValidationError),
}

impl WakeRequestError {
    pub fn field(&self) -> &'static str {
        match self {
            Self::Debounce | Self::DebounceRange => "debounce",
            Self::Request(e) => e.field(),
        }
    }
}

impl fmt::Display for WakeRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Debounce => f.write_str(
                "debounce must be a whole number followed by s, m, h or d, e.g. \"15m\"",
            ),
            Self::DebounceRange => write!(
                f,
                "debounce must be between {} minute and 24 hours",
                MIN_WAKE_DEBOUNCE_MS / MINUTE_MS
            ),
            Self::Request(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for WakeRequestError {}

impl WakeRequest {
    pub fn validate(self, now_ms: i64) -> Result<WakeSpec, WakeRequestError> {
        let debounce = self.debounce.as_deref().unwrap_or(DEFAULT_WAKE_DEBOUNCE);
        let debounce_ms = parse_delay(debounce).ok_or(WakeRequestError::Debounce)?;
        if !(MIN_WAKE_DEBOUNCE_MS..=MAX_WAKE_DEBOUNCE_MS).contains(&debounce_ms) {
            return Err(WakeRequestError::DebounceRange);
        }
        // The body is RunLater's own; headers and the URL follow schedule_http's rules.
        let request = ScheduleRequest {
            url: self.url,
            method: Method::Post,
            headers: self.headers,
            ..Default::default()
        }
        .validate(now_ms)
        .map_err(WakeRequestError::Request)?;
        Ok(WakeSpec {
            request,
            debounce_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::HOUR_MS;

    const NOW: i64 = 1_767_225_600_000;

    fn wake(extra: serde_json::Value) -> WakeRequest {
        let mut r = serde_json::json!({ "url": "https://hooks.example.com/wake" });
        r.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(r).unwrap()
    }

    #[test]
    fn defaults_to_a_fifteen_minute_debounce() {
        let spec = wake(serde_json::json!({})).validate(NOW).unwrap();
        assert_eq!(spec.debounce_ms, 15 * MINUTE_MS);
        assert_eq!(spec.request.method, Method::Post);
        assert_eq!(
            spec.request.destination.as_str(),
            "https://hooks.example.com/wake"
        );
        let hourly = wake(serde_json::json!({ "debounce": "1h" }))
            .validate(NOW)
            .unwrap();
        assert_eq!(hourly.debounce_ms, HOUR_MS);
    }

    #[test]
    fn checks_debounce_url_and_headers() {
        let err = |extra| wake(extra).validate(NOW).unwrap_err();
        assert_eq!(
            err(serde_json::json!({ "debounce": "soon" })),
            WakeRequestError::Debounce
        );
        for d in ["59s", "25h", "2d"] {
            assert_eq!(
                err(serde_json::json!({ "debounce": d })),
                WakeRequestError::DebounceRange
            );
        }
        assert!(wake(serde_json::json!({ "debounce": "1m" }))
            .validate(NOW)
            .is_ok());
        assert!(wake(serde_json::json!({ "debounce": "24h" }))
            .validate(NOW)
            .is_ok());
        assert_eq!(
            err(serde_json::json!({ "url": "http://x.example.com/" })).field(),
            "url"
        );
        assert_eq!(
            err(serde_json::json!({ "headers": { "Host": "evil.example.com" } })).field(),
            "headers"
        );
        assert!(serde_json::from_value::<WakeRequest>(
            serde_json::json!({ "url": "https://a.example.com/", "body": "x" })
        )
        .is_err());
    }
}
