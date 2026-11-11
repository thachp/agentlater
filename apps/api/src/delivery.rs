//! One delivery attempt, from a claimed [`Delivery`] to what the ledger records.
//!
//! The attempt fills the task's secret references with values opened for this attempt only,
//! signs the request, sends it through a [`Transport`], and settles the answer: any secret
//! value the response echoes is redacted before the body is cut to its cap, so a value that
//! straddles the cap is redacted whole. A task whose secrets can't be opened or filled, or
//! whose URL the egress policy refuses, is rejected without being sent.
//!
//! The Workers runtime sends for real; tests script the destination's answers.

use crate::ledger::Delivery;
use runlater_core::limits::{CAPTURE_BODY_BYTES, MAX_SECRET_BYTES, MAX_WATCH_RESPONSE_BYTES};
use runlater_core::{fill, redact, Method, Outcome};
use runlater_egress::{sign, Destination};

pub const USER_AGENT: &str = "AgentLater/1 (+https://agentlater.io)";

/// How much of the response an attempt reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadPlan {
    /// The status and `Retry-After` only.
    Status,
    /// A task with `capture`: up to `max_bytes` of the body and a few headers, but only while
    /// the attempt has time left. The destination has answered, so a slow or broken body must
    /// not change the outcome; it is reported unread instead.
    Capture { max_bytes: usize },
    /// A watch's poll: up to `max_bytes` of the body. A body cut off mid-read is a failed
    /// poll ([`Outcome::Network`]), not an empty answer.
    Poll { max_bytes: usize },
}

/// A request ready to send, secrets filled in and signed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub method: Method,
    pub url: String,
    /// In the order to set them: a later header replaces an earlier one of the same name.
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub read: ReadPlan,
}

/// A response as read for a capture or a poll.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponseBody {
    /// None when a captured body could not be read in time.
    pub body: Option<String>,
    /// Whether the body was cut short.
    pub truncated: bool,
    /// With capture: the headers worth keeping, `content-length` and `content-encoding`.
    pub headers: Vec<(String, String)>,
}

/// Sends a request and reads its answer as `request.read` says, all within the attempt's
/// timeout: [`Outcome::Http`] for any answer, [`Outcome::Timeout`] or [`Outcome::Network`]
/// otherwise. A body is returned only for a capture or a poll.
#[allow(async_fn_in_trait)]
pub trait Transport {
    async fn send(&self, request: &Outgoing) -> (Outcome, Option<ResponseBody>);
}

/// What the ledger records for an attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    pub outcome: Outcome,
    pub body: Option<String>,
    pub truncated: bool,
    pub headers: Vec<(String, String)>,
}

impl Settled {
    /// An outcome with nothing read back.
    fn bare(outcome: Outcome) -> Self {
        Settled {
            outcome,
            body: None,
            truncated: false,
            headers: vec![],
        }
    }
}

/// Makes the attempt. `opened` holds the plaintext of each of `d.secrets`, or is None when
/// they could not be opened; `now_s` is the signature's timestamp.
pub async fn attempt(
    d: &Delivery,
    opened: Option<Vec<(String, String)>>,
    now_s: i64,
    transport: &impl Transport,
) -> Settled {
    let values = opened.unwrap_or_default();
    if values.len() < d.secrets.len() {
        return Settled::bare(Outcome::Rejected);
    }
    let Some(request) = prepare(d, &values, now_s) else {
        return Settled::bare(Outcome::Rejected);
    };
    let (outcome, read) = transport.send(&request).await;
    settle(d, &values, outcome, read)
}

fn prepare(d: &Delivery, values: &[(String, String)], now_s: i64) -> Option<Outgoing> {
    let lookup = |name: &str| {
        values
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };
    let filled = d
        .headers
        .iter()
        .map(|(k, v)| Some((k.clone(), fill(v, lookup)?)))
        .collect::<Option<Vec<_>>>()?;
    // Policy is checked again on every attempt; the provider separately refuses private
    // addresses after DNS resolution.
    let destination = Destination::parse(&d.url).ok()?;
    let body = d.body.clone().unwrap_or_default();
    let secrets: Vec<&[u8]> = d.signing_secrets.iter().map(String::as_bytes).collect();
    let signature = sign(&secrets, &d.task_id, d.attempt, now_s, body.as_bytes());
    let headers = [("User-Agent".to_string(), USER_AGENT.to_string())]
        .into_iter()
        .chain(filled)
        .chain(
            signature
                .headers()
                .into_iter()
                .map(|(k, v)| (k.to_string(), v)),
        )
        .collect();
    // Read a little past the cap when there are secrets, so an echoed value that straddles
    // it is still whole when it is redacted.
    let margin = if d.secrets.is_empty() {
        0
    } else {
        MAX_SECRET_BYTES
    };
    let read = match (d.read_body, d.capture) {
        (false, _) => ReadPlan::Status,
        (true, true) => ReadPlan::Capture {
            max_bytes: CAPTURE_BODY_BYTES + margin,
        },
        (true, false) => ReadPlan::Poll {
            max_bytes: MAX_WATCH_RESPONSE_BYTES + margin,
        },
    };
    Some(Outgoing {
        method: d.method,
        url: destination.as_str().to_string(),
        headers,
        body: d.body.clone(),
        read,
    })
}

fn settle(
    d: &Delivery,
    values: &[(String, String)],
    outcome: Outcome,
    read: Option<ResponseBody>,
) -> Settled {
    let cap = if d.capture {
        CAPTURE_BODY_BYTES
    } else {
        MAX_WATCH_RESPONSE_BYTES
    };
    let Some(read) = read else {
        return Settled::bare(outcome);
    };
    // Redact before cutting: the body was read past its cap for exactly this.
    let (body, cut) = match read.body.map(|b| redact(&b, values)) {
        Some(b) if b.len() > cap => (Some(cut_at(&b, cap)), true),
        other => (other, false),
    };
    Settled {
        outcome,
        body,
        truncated: read.truncated || cut,
        headers: read
            .headers
            .into_iter()
            .map(|(k, v)| (k, redact(&v, values)))
            .collect(),
    }
}

/// The first `max` bytes of `s`, cut at a character boundary.
fn cut_at(s: &str, max: usize) -> String {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::SealedSecret;
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    /// The scripted destination: answers every request the same way and remembers them.
    struct Script {
        answer: (Outcome, Option<ResponseBody>),
        sent: RefCell<Vec<Outgoing>>,
    }

    impl Script {
        fn answering(outcome: Outcome, read: Option<ResponseBody>) -> Self {
            Script {
                answer: (outcome, read),
                sent: RefCell::new(vec![]),
            }
        }
    }

    impl Transport for Script {
        async fn send(&self, request: &Outgoing) -> (Outcome, Option<ResponseBody>) {
            self.sent.borrow_mut().push(request.clone());
            self.answer.clone()
        }
    }

    /// Runs a future that never waits, which is every future here.
    fn run<F: Future>(f: F) -> F::Output {
        let mut f = pin!(f);
        match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(out) => out,
            Poll::Pending => panic!("the scripted transport never waits"),
        }
    }

    const OK: Outcome = Outcome::Http {
        status: 200,
        retry_after_ms: None,
    };

    fn delivery() -> Delivery {
        Delivery {
            task_id: "t1".into(),
            attempt: 1,
            fence: 1,
            method: Method::Post,
            url: "https://api.example.com/hook".into(),
            headers: vec![("X-Plain".into(), "yes".into())],
            body: Some("{}".into()),
            signing_secrets: vec!["whsec".into()],
            read_body: false,
            capture: false,
            secrets: vec![],
            late_ms: None,
        }
    }

    fn with_secret(mut d: Delivery) -> Delivery {
        d.headers
            .push(("Authorization".into(), "Bearer {{secret:GH}}".into()));
        d.secrets = vec![SealedSecret {
            name: "GH".into(),
            sealed: "sealed".into(),
        }];
        d
    }

    fn gh(value: &str) -> Option<Vec<(String, String)>> {
        Some(vec![("GH".into(), value.into())])
    }

    fn body(text: String) -> Option<ResponseBody> {
        Some(ResponseBody {
            body: Some(text),
            ..Default::default()
        })
    }

    #[test]
    fn sends_signed_with_our_user_agent_and_reads_only_the_status() {
        let script = Script::answering(OK, None);
        let settled = run(attempt(&delivery(), None, 1_700_000_000, &script));
        assert_eq!(settled.outcome, OK);
        let sent = script.sent.borrow();
        let names: Vec<&str> = sent[0].headers.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names[..2], ["User-Agent", "X-Plain"]);
        assert_eq!(names.len(), 6, "four signature headers follow: {names:?}");
        assert_eq!(sent[0].read, ReadPlan::Status);
        assert_eq!(sent[0].body.as_deref(), Some("{}"));
    }

    #[test]
    fn fills_secrets_for_the_send_only() {
        let script = Script::answering(OK, None);
        run(attempt(&with_secret(delivery()), gh("s3cret"), 0, &script));
        let sent = script.sent.borrow();
        assert!(sent[0]
            .headers
            .contains(&("Authorization".into(), "Bearer s3cret".into())));
    }

    #[test]
    fn rejects_without_sending_when_secrets_cannot_be_used() {
        let script = Script::answering(OK, None);
        let d = with_secret(delivery());
        assert_eq!(
            run(attempt(&d, None, 0, &script)),
            Settled::bare(Outcome::Rejected)
        );
        assert_eq!(
            run(attempt(&d, Some(vec![]), 0, &script)),
            Settled::bare(Outcome::Rejected)
        );
        assert_eq!(
            run(attempt(
                &d,
                Some(vec![("OTHER".into(), "x".into())]),
                0,
                &script
            )),
            Settled::bare(Outcome::Rejected),
            "a reference to a secret that wasn't opened"
        );
        let mut private = delivery();
        private.url = "http://127.0.0.1/".into();
        assert_eq!(
            run(attempt(&private, None, 0, &script)),
            Settled::bare(Outcome::Rejected)
        );
        assert!(script.sent.borrow().is_empty());
    }

    #[test]
    fn reads_past_the_cap_only_when_there_are_secrets() {
        let script = Script::answering(OK, body(String::new()));
        let mut poll = delivery();
        poll.read_body = true;
        run(attempt(&poll, None, 0, &script));
        run(attempt(&with_secret(poll.clone()), gh("v"), 0, &script));
        let mut capture = poll;
        capture.capture = true;
        run(attempt(&capture, None, 0, &script));
        let plans: Vec<ReadPlan> = script.sent.borrow().iter().map(|r| r.read).collect();
        assert_eq!(
            plans,
            [
                ReadPlan::Poll {
                    max_bytes: MAX_WATCH_RESPONSE_BYTES
                },
                ReadPlan::Poll {
                    max_bytes: MAX_WATCH_RESPONSE_BYTES + MAX_SECRET_BYTES
                },
                ReadPlan::Capture {
                    max_bytes: CAPTURE_BODY_BYTES
                },
            ]
        );
    }

    #[test]
    fn an_echoed_secret_straddling_the_cap_is_redacted_whole_before_the_cut() {
        let secret = "s3cret-value";
        let mut d = with_secret(delivery());
        d.read_body = true;
        d.capture = true;
        // The secret starts four bytes before the cap and runs past it.
        let echoed = format!("{}{secret}tail", "a".repeat(CAPTURE_BODY_BYTES - 4));
        let script = Script::answering(
            OK,
            Some(ResponseBody {
                body: Some(echoed),
                truncated: false,
                headers: vec![("x-echo".into(), format!("got {secret}"))],
            }),
        );
        let settled = run(attempt(&d, gh(secret), 0, &script));
        let kept = settled.body.unwrap();
        assert_eq!(kept.len(), CAPTURE_BODY_BYTES);
        assert!(
            !kept.contains("s3cr"),
            "no part of the secret survives the cut"
        );
        assert!(settled.truncated);
        assert!(!settled.headers[0].1.contains(secret));
    }

    #[test]
    fn a_short_answer_is_kept_whole_and_failures_pass_through() {
        let mut d = delivery();
        d.read_body = true;
        let script = Script::answering(OK, body("done".into()));
        let settled = run(attempt(&d, None, 0, &script));
        assert_eq!(
            (settled.body.as_deref(), settled.truncated),
            (Some("done"), false)
        );

        let script = Script::answering(Outcome::Timeout, None);
        let settled = run(attempt(&d, None, 0, &script));
        assert_eq!(settled, Settled::bare(Outcome::Timeout));
    }

    #[test]
    fn cuts_at_a_character_boundary() {
        assert_eq!(cut_at("héllo", 2), "h");
        assert_eq!(cut_at("abc", 10), "abc");
    }
}
