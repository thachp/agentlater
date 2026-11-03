//! `watch_http`: poll a URL until its response matches a condition, a deadline passes, or
//! polling keeps failing.
//!
//! A watch is an ordinary task whose attempts are polls. After a poll that does not match,
//! the task waits for the next poll instead of retrying, so `max_attempts` never applies;
//! [`judge`] decides each poll and [`next_poll`] when the next one runs.

use crate::limits::{
    MAX_POLL_ERRORS, MAX_SCHEDULE_AHEAD_MS, MAX_WATCH_INTERVAL_MS, MINUTE_MS, MIN_WATCH_INTERVAL_MS,
};
use crate::request::{parse_delay, Callback, Method, ScheduleRequest, TaskSpec, ValidationError};
use crate::retry::{classify, Outcome, Verdict};
use runlater_egress::Destination;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;

const MAX_POINTER_BYTES: usize = 256;
const MAX_UNTIL_VALUES: usize = 20;
const MAX_UNTIL_STRING_BYTES: usize = 256;

fn get() -> Method {
    Method::Get
}

/// `watch_http` input exactly as the caller sent it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchRequest {
    pub url: String,
    #[serde(default = "get")]
    pub method: Method,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
    /// Time between polls, e.g. `"5m"`; the first poll is immediate.
    pub every: String,
    pub until: Until,
    /// How long to keep polling, e.g. `"6h"`.
    pub deadline: String,
    /// Where to POST the watch's final state, or `"inbox"`.
    #[serde(default)]
    pub callback_url: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// Sent once, as an ordinary task, when `until` matches.
    #[serde(default)]
    pub on_match: Option<OnMatch>,
}

/// The request a watch sends when it matches: `schedule_http`'s fields, less the ones about
/// when (it goes as soon as the watch matches) and ordering. It can't start another watch,
/// so a watch triggers one request at most.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnMatch {
    pub url: String,
    #[serde(default)]
    pub method: Method,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub max_attempts: Option<u32>,
    /// Where the action reports when it finishes, or `"inbox"`.
    #[serde(default)]
    pub callback_url: Option<String>,
    #[serde(default)]
    pub capture: bool,
}

/// When a poll's response counts as done. With `path` and `in`, a 2xx response whose JSON
/// body has one of the values at that JSON Pointer matches. With `status`, the response
/// status must be one of those. Given both, both must hold.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Until {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, rename = "in", skip_serializing_if = "Option::is_none")]
    pub one_of: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Vec<u16>>,
}

/// A watch's settings as stored with its task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watch {
    pub every_ms: i64,
    pub deadline_ms: i64,
    pub until: Until,
}

/// A validated watch, ready to become a task. `request.fingerprint` covers the whole watch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchSpec {
    pub request: TaskSpec,
    pub watch: Watch,
    /// The task to queue when the watch matches. Its `run_at_ms` is meaningless here.
    pub on_match: Option<TaskSpec>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchRequestError {
    Every,
    EveryRange,
    Deadline,
    DeadlineRange,
    Until(&'static str),
    Request(ValidationError),
    OnMatch(ValidationError),
}

impl WatchRequestError {
    pub fn field(&self) -> &'static str {
        match self {
            Self::Every | Self::EveryRange => "every",
            Self::Deadline | Self::DeadlineRange => "deadline",
            Self::Until(_) => "until",
            Self::Request(e) => e.field(),
            Self::OnMatch(_) => "on_match",
        }
    }
}

impl fmt::Display for WatchRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Every => {
                f.write_str("every must be a whole number followed by s, m, h or d, e.g. \"5m\"")
            }
            Self::EveryRange => write!(
                f,
                "every must be between {} minutes and 24 hours",
                MIN_WATCH_INTERVAL_MS / MINUTE_MS
            ),
            Self::Deadline => {
                f.write_str("deadline must be a whole number followed by s, m, h or d, e.g. \"6h\"")
            }
            Self::DeadlineRange => {
                f.write_str("deadline must be at least one interval and at most 7 days")
            }
            Self::Until(why) => write!(f, "until: {why}"),
            Self::Request(e) => e.fmt(f),
            Self::OnMatch(e) => write!(f, "on_match.{}: {e}", e.field()),
        }
    }
}

impl std::error::Error for WatchRequestError {}

impl WatchRequest {
    pub fn validate(self, now_ms: i64) -> Result<WatchSpec, WatchRequestError> {
        let every_ms = parse_delay(&self.every).ok_or(WatchRequestError::Every)?;
        if !(MIN_WATCH_INTERVAL_MS..=MAX_WATCH_INTERVAL_MS).contains(&every_ms) {
            return Err(WatchRequestError::EveryRange);
        }
        let deadline = parse_delay(&self.deadline).ok_or(WatchRequestError::Deadline)?;
        if !(every_ms..=MAX_SCHEDULE_AHEAD_MS).contains(&deadline) {
            return Err(WatchRequestError::DeadlineRange);
        }
        self.until.check().map_err(WatchRequestError::Until)?;
        let on_match = self
            .on_match
            .clone()
            .map(|a| {
                ScheduleRequest {
                    url: a.url,
                    method: a.method,
                    headers: a.headers,
                    body: a.body,
                    max_attempts: a.max_attempts,
                    callback_url: a.callback_url,
                    capture: a.capture,
                    ..Default::default()
                }
                .validate(now_ms)
            })
            .transpose()
            .map_err(WatchRequestError::OnMatch)?;

        let mut request = ScheduleRequest {
            url: self.url.clone(),
            method: self.method,
            headers: self.headers.clone(),
            body: self.body.clone(),
            callback_url: self.callback_url.clone(),
            idempotency_key: self.idempotency_key.clone(),
            ..Default::default()
        }
        .validate(now_ms)
        .map_err(WatchRequestError::Request)?;
        request.max_attempts = 1;
        request.fingerprint = self.fingerprint(&request.destination, request.callback.as_ref());
        Ok(WatchSpec {
            request,
            watch: Watch {
                every_ms,
                deadline_ms: now_ms.saturating_add(deadline),
                until: self.until,
            },
            on_match,
        })
    }

    /// Like `schedule_http`'s: normalized URLs and lowercased header names, and the
    /// *submitted* durations, so a retried call replays.
    fn fingerprint(&self, destination: &Destination, callback: Option<&Callback>) -> String {
        let headers: BTreeMap<_, _> = self
            .headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect();
        let watch = (
            "watch_http/v1",
            destination.as_str(),
            self.method,
            headers,
            &self.body,
            &self.every,
            &self.until,
            &self.deadline,
            callback.map(Callback::as_str),
        );
        // Watches without on_match keep the fingerprint they always had.
        let canonical = match &self.on_match {
            None => serde_json::to_vec(&watch),
            Some(action) => serde_json::to_vec(&(watch, action)),
        }
        .expect("plain data always serializes");
        hex::encode(Sha256::digest(canonical))
    }
}

impl Until {
    fn check(&self) -> Result<(), &'static str> {
        match (&self.path, &self.one_of) {
            (Some(path), Some(values)) => {
                if !valid_pointer(path) {
                    return Err("path must be a JSON Pointer such as \"/status\"");
                }
                if values.is_empty() || values.len() > MAX_UNTIL_VALUES {
                    return Err("in must list 1-20 values");
                }
                let scalar = |v: &Value| match v {
                    Value::String(s) => s.len() <= MAX_UNTIL_STRING_BYTES,
                    Value::Null | Value::Bool(_) | Value::Number(_) => true,
                    _ => false,
                };
                if !values.iter().all(scalar) {
                    return Err(
                        "in values must be strings (up to 256 bytes), numbers, booleans or null",
                    );
                }
            }
            (None, None) => {}
            _ => return Err("path and in go together"),
        }
        match &self.status {
            Some(codes) if codes.is_empty() || codes.len() > MAX_UNTIL_VALUES => {
                Err("status must list 1-20 codes")
            }
            Some(codes) if !codes.iter().all(|c| (100..=599).contains(c)) => {
                Err("status codes must be between 100 and 599")
            }
            None if self.path.is_none() => Err("give path and in, or status"),
            _ => Ok(()),
        }
    }

    /// The value that satisfied the condition (the matched JSON value, or the status), if
    /// this response does.
    pub fn matches(&self, status: u16, body: Option<&str>) -> Option<Value> {
        if !self.status_allows(status) {
            return None;
        }
        match (&self.path, &self.one_of) {
            (Some(path), Some(values)) => {
                let doc: Value = serde_json::from_str(body?).ok()?;
                let found = doc.pointer(path)?;
                // Numbers compare by value, so 3 matches 3.0.
                let same = |v: &Value| match (v.as_f64(), found.as_f64()) {
                    (Some(a), Some(b)) => a == b,
                    _ => v == found,
                };
                values.iter().any(same).then(|| found.clone())
            }
            _ => Some(Value::from(status)),
        }
    }
}

impl Until {
    /// Whether a response with this status can match: one of `status`, or any 2xx.
    fn status_allows(&self, status: u16) -> bool {
        match &self.status {
            Some(codes) => codes.contains(&status),
            None => (200..=299).contains(&status),
        }
    }
}

/// RFC 6901: empty (the whole document), or `/`-separated tokens where `~` only appears as
/// `~0` or `~1`.
fn valid_pointer(path: &str) -> bool {
    if path.len() > MAX_POINTER_BYTES || !(path.is_empty() || path.starts_with('/')) {
        return false;
    }
    let bytes = path.as_bytes();
    bytes
        .iter()
        .enumerate()
        .all(|(i, b)| *b != b'~' || matches!(bytes.get(i + 1), Some(b'0' | b'1')))
}

/// What one poll decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Poll {
    /// Done: the value that matched.
    Matched(Value),
    /// Poll again later.
    NotYet,
    /// Stop polling: `poll_failing` (too many failed polls in a row), `poll_rejected` (an
    /// answer that will not change, such as a 404 or a redirect) or `response_too_large` (the
    /// condition needs a body longer than RunLater reads).
    Fail(&'static str),
}

/// A polled response's body as read: at most [`MAX_WATCH_RESPONSE_BYTES`], and whether
/// there was more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollBody<'a> {
    pub text: &'a str,
    pub truncated: bool,
}

/// Judges a poll's result. `errors` is how many polls in a row had failed before this one;
/// returns the verdict and the new count.
pub fn judge(
    until: &Until,
    outcome: &Outcome,
    body: Option<PollBody<'_>>,
    errors: u32,
) -> (Poll, u32) {
    if let Outcome::Http { status, .. } = outcome {
        // A cut-off body never parses, so a path condition could never match it.
        let unreadable = body.is_some_and(|b| b.truncated) && until.path.is_some();
        if unreadable && until.status_allows(*status) {
            return (Poll::Fail("response_too_large"), errors);
        }
        if let Some(value) = until.matches(*status, body.map(|b| b.text)) {
            return (Poll::Matched(value), 0);
        }
    }
    match classify(outcome) {
        Verdict::Success => (Poll::NotYet, 0),
        Verdict::Retry if errors + 1 >= MAX_POLL_ERRORS => (Poll::Fail("poll_failing"), errors + 1),
        Verdict::Retry => (Poll::NotYet, errors + 1),
        Verdict::GiveUp => (Poll::Fail("poll_rejected"), errors),
    }
}

/// When to poll next, or `None` if the deadline has passed. The last poll is at the
/// deadline itself.
pub fn next_poll(now_ms: i64, watch: &Watch) -> Option<i64> {
    (now_ms < watch.deadline_ms)
        .then(|| now_ms.saturating_add(watch.every_ms).min(watch.deadline_ms))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::{DAY_MS, HOUR_MS};
    use serde_json::json;

    const T0: i64 = 1_767_225_600_000;

    fn http(status: u16) -> Outcome {
        Outcome::Http {
            status,
            retry_after_ms: None,
        }
    }

    fn until_status_done() -> Until {
        Until {
            path: Some("/status".into()),
            one_of: Some(vec![json!("succeeded"), json!("failed")]),
            status: None,
        }
    }

    fn req() -> WatchRequest {
        serde_json::from_value(json!({
            "url": "https://api.example.com/jobs/1",
            "every": "5m",
            "until": { "path": "/status", "in": ["succeeded", "failed"] },
            "deadline": "6h",
        }))
        .unwrap()
    }

    #[test]
    fn validates_a_watch() {
        let spec = req().validate(T0).unwrap();
        assert_eq!(spec.request.method, Method::Get);
        assert_eq!(spec.request.run_at_ms, T0);
        assert_eq!(spec.request.max_attempts, 1);
        assert_eq!(spec.watch.every_ms, 5 * MINUTE_MS);
        assert_eq!(spec.watch.deadline_ms, T0 + 6 * HOUR_MS);
        assert_eq!(spec.watch.until, until_status_done());
    }

    #[test]
    fn rejects_bad_timing() {
        for (every, deadline, field) in [
            ("5", "6h", "every"),
            ("4m", "6h", "every"),
            ("25h", "6d", "every"),
            ("5m", "6", "deadline"),
            ("1h", "30m", "deadline"),
            ("5m", "8d", "deadline"),
        ] {
            let r = WatchRequest {
                every: every.into(),
                deadline: deadline.into(),
                ..req()
            };
            assert_eq!(
                r.validate(T0).unwrap_err().field(),
                field,
                "{every} {deadline}"
            );
        }
        let r = WatchRequest {
            every: "1d".into(),
            deadline: "7d".into(),
            ..req()
        };
        assert_eq!(r.validate(T0).unwrap().watch.deadline_ms, T0 + 7 * DAY_MS);
    }

    #[test]
    fn rejects_bad_conditions() {
        for until in [
            json!({}),
            json!({ "path": "/status" }),
            json!({ "in": ["done"] }),
            json!({ "path": "status", "in": ["done"] }),
            json!({ "path": "/a~2", "in": ["done"] }),
            json!({ "path": "/status", "in": [] }),
            json!({ "path": "/status", "in": [{ "nested": true }] }),
            json!({ "status": [] }),
            json!({ "status": [600] }),
        ] {
            let mut r = req();
            r.until = serde_json::from_value(until.clone()).unwrap();
            assert_eq!(r.validate(T0).unwrap_err().field(), "until", "{until}");
        }
        assert!(serde_json::from_value::<Until>(json!({ "regex": "x" })).is_err());
        let mut r = req();
        r.until = serde_json::from_value(json!({ "status": [200, 404] })).unwrap();
        assert!(r.validate(T0).is_ok());
    }

    #[test]
    fn request_errors_name_their_field() {
        let mut r = req();
        r.url = "http://insecure.example.com/".into();
        assert_eq!(r.validate(T0).unwrap_err().field(), "url");
        let mut r = req();
        r.body = Some("x".into());
        assert_eq!(r.validate(T0).unwrap_err().field(), "body");
    }

    fn with_on_match(action: Value) -> WatchRequest {
        let mut r = req();
        r.on_match = Some(serde_json::from_value(action).unwrap());
        r
    }

    #[test]
    fn validates_on_match_as_an_ordinary_request() {
        assert_eq!(req().validate(T0).unwrap().on_match, None);
        let spec = with_on_match(json!({
            "url": "https://ci.example.com/smoke",
            "headers": { "Authorization": "{{secret:CI_TOKEN}}" },
            "body": "{\"deployment\":1}",
            "callback_url": "inbox",
            "capture": true,
        }))
        .validate(T0)
        .unwrap();
        let action = spec.on_match.unwrap();
        assert_eq!(action.method, Method::Post, "POST, as schedule_http");
        assert_eq!(action.destination.as_str(), "https://ci.example.com/smoke");
        assert_eq!(action.callback, Some(Callback::Inbox));
        assert!(action.capture);
        assert_eq!(
            spec.request.callback, None,
            "the watch's own callback is apart"
        );
    }

    #[test]
    fn rejects_bad_on_match() {
        for action in [
            json!({ "url": "http://ci.example.com/" }),
            json!({ "url": "https://ci.example.com/", "method": "GET", "body": "x" }),
            json!({ "url": "https://ci.example.com/", "body": "{{secret:CI_TOKEN}}" }),
            json!({ "url": "https://ci.example.com/", "max_attempts": 0 }),
            json!({ "url": "https://ci.example.com/", "callback_url": "http://x.example.com/" }),
        ] {
            let err = with_on_match(action.clone()).validate(T0).unwrap_err();
            assert_eq!(err.field(), "on_match", "{action}");
            assert!(err.to_string().starts_with("on_match."), "{err}");
        }
        // Timing, ordering and nesting are not on_match's to set.
        for extra in [
            "delay",
            "run_at_ms",
            "lane",
            "idempotency_key",
            "on_match",
            "every",
        ] {
            let mut action = json!({ "url": "https://ci.example.com/" });
            action[extra] = json!("x");
            assert!(
                serde_json::from_value::<OnMatch>(action).is_err(),
                "{extra}"
            );
        }
    }

    #[test]
    fn on_match_is_part_of_the_fingerprint() {
        let plain = req().validate(T0).unwrap().request.fingerprint;
        let a = with_on_match(json!({ "url": "https://ci.example.com/a" }));
        let b = with_on_match(json!({ "url": "https://ci.example.com/b" }));
        let fa = a.clone().validate(T0).unwrap().request.fingerprint;
        assert_ne!(plain, fa);
        assert_ne!(fa, b.validate(T0).unwrap().request.fingerprint);
        assert_eq!(fa, a.validate(T0 + 1_000).unwrap().request.fingerprint);
    }

    #[test]
    fn fingerprints_replay_and_differ() {
        let a = req().validate(T0).unwrap().request.fingerprint;
        assert_eq!(a, req().validate(T0 + 1_000).unwrap().request.fingerprint);
        let mut other = req();
        other.every = "10m".into();
        assert_ne!(a, other.validate(T0).unwrap().request.fingerprint);
        let mut other = req();
        other.until.one_of = Some(vec![json!("succeeded")]);
        assert_ne!(a, other.validate(T0).unwrap().request.fingerprint);
    }

    #[test]
    fn matches_json_values_on_2xx_only() {
        let u = until_status_done();
        let done = r#"{"status":"succeeded","id":1}"#;
        assert_eq!(u.matches(200, Some(done)), Some(json!("succeeded")));
        assert_eq!(u.matches(200, Some(r#"{"status":"running"}"#)), None);
        assert_eq!(u.matches(200, Some("not json")), None);
        assert_eq!(u.matches(200, None), None);
        assert_eq!(u.matches(500, Some(done)), None);

        let nested = Until {
            path: Some("/job/state".into()),
            one_of: Some(vec![json!(3), json!(true)]),
            status: None,
        };
        assert_eq!(
            nested.matches(200, Some(r#"{"job":{"state":3}}"#)),
            Some(json!(3))
        );
        assert_eq!(nested.matches(200, Some(r#"{"job":{"state":"3"}}"#)), None);
    }

    #[test]
    fn matches_status_codes() {
        let gone = Until {
            status: Some(vec![404]),
            ..Default::default()
        };
        assert_eq!(gone.matches(404, None), Some(json!(404)));
        assert_eq!(gone.matches(200, None), None);

        let both = Until {
            status: Some(vec![200]),
            ..until_status_done()
        };
        assert_eq!(
            both.matches(200, Some(r#"{"status":"failed"}"#)),
            Some(json!("failed"))
        );
        assert_eq!(both.matches(201, Some(r#"{"status":"failed"}"#)), None);
    }

    fn text(s: &str) -> Option<PollBody<'_>> {
        Some(PollBody {
            text: s,
            truncated: false,
        })
    }

    #[test]
    fn judges_polls() {
        let u = until_status_done();
        let running = text(r#"{"status":"running"}"#);
        assert_eq!(judge(&u, &http(200), running, 2), (Poll::NotYet, 0));
        assert_eq!(
            judge(&u, &http(200), text(r#"{"status":"failed"}"#), 0),
            (Poll::Matched(json!("failed")), 0)
        );
        assert_eq!(judge(&u, &http(503), None, 0), (Poll::NotYet, 1));
        assert_eq!(judge(&u, &Outcome::Timeout, None, 1), (Poll::NotYet, 2));
        assert_eq!(
            judge(&u, &Outcome::LeaseExpired, None, 2),
            (Poll::Fail("poll_failing"), 3)
        );
        assert_eq!(
            judge(&u, &http(404), None, 1),
            (Poll::Fail("poll_rejected"), 1)
        );
        assert_eq!(
            judge(&u, &http(302), None, 0),
            (Poll::Fail("poll_rejected"), 0)
        );
        assert_eq!(
            judge(&u, &Outcome::Rejected, None, 0),
            (Poll::Fail("poll_rejected"), 0)
        );

        // A status condition can wait for what would otherwise be a failure.
        let gone = Until {
            status: Some(vec![404]),
            ..Default::default()
        };
        assert_eq!(
            judge(&gone, &http(404), None, 0),
            (Poll::Matched(json!(404)), 0)
        );
        assert_eq!(judge(&gone, &http(200), None, 0), (Poll::NotYet, 0));
    }

    #[test]
    fn a_cut_off_body_ends_a_watch_that_needs_it() {
        let cut = Some(PollBody {
            text: r#"{"status":"succeeded","log":"#,
            truncated: true,
        });
        assert_eq!(
            judge(&until_status_done(), &http(200), cut, 1),
            (Poll::Fail("response_too_large"), 1)
        );
        // Not a 2xx: the body was never going to be looked at.
        assert_eq!(
            judge(&until_status_done(), &http(503), cut, 0),
            (Poll::NotYet, 1)
        );
        // A status-only condition does not need the body.
        let ok = Until {
            status: Some(vec![200]),
            ..Default::default()
        };
        assert_eq!(
            judge(&ok, &http(200), cut, 0),
            (Poll::Matched(serde_json::json!(200)), 0)
        );
    }

    #[test]
    fn numbers_match_by_value() {
        let u = Until {
            path: Some("/progress".into()),
            one_of: Some(vec![serde_json::json!(100)]),
            status: None,
        };
        assert!(u.matches(200, Some(r#"{"progress":100.0}"#)).is_some());
        assert!(u.matches(200, Some(r#"{"progress":"100"}"#)).is_none());
    }

    #[test]
    fn polls_until_the_deadline_then_stops() {
        let w = Watch {
            every_ms: 5 * MINUTE_MS,
            deadline_ms: T0 + 12 * MINUTE_MS,
            until: until_status_done(),
        };
        assert_eq!(next_poll(T0, &w), Some(T0 + 5 * MINUTE_MS));
        assert_eq!(
            next_poll(T0 + 10 * MINUTE_MS, &w),
            Some(T0 + 12 * MINUTE_MS)
        );
        assert_eq!(next_poll(T0 + 12 * MINUTE_MS, &w), None);
    }
}
