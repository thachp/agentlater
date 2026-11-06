use crate::limits::{RETRY_CAP_MS, RETRY_DELAYS_MS};
use serde::{Deserialize, Serialize};

/// What happened on one delivery attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    /// The destination answered. `retry_after_ms` comes from a `Retry-After` header.
    Http {
        status: u16,
        retry_after_ms: Option<i64>,
    },
    /// No response within the attempt timeout. The request may have had an effect.
    Timeout,
    /// Connection or TLS failure before a response.
    Network,
    /// AgentLater refused to send, or refused the response (destination failed policy on
    /// re-check, redirect, oversized response).
    Rejected,
    /// The worker holding the attempt vanished. The request may have had an effect.
    LeaseExpired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Success,
    Retry,
    GiveUp,
}

impl Outcome {
    /// Short stable label for attempt history, e.g. `http_503`.
    pub fn label(&self) -> String {
        match self {
            Self::Http { status, .. } => format!("http_{status}"),
            Self::Timeout => "timeout".into(),
            Self::Network => "network_error".into(),
            Self::Rejected => "rejected".into(),
            Self::LeaseExpired => "lease_expired".into(),
        }
    }
}

/// 2xx succeeds. 408, 425, 429, 5xx, timeouts and lost workers retry. Everything else
/// (other 4xx, redirects, policy rejections) gives up: retrying would not change it.
pub fn classify(outcome: &Outcome) -> Verdict {
    match outcome {
        Outcome::Http { status, .. } => match status {
            200..=299 => Verdict::Success,
            408 | 425 | 429 | 500..=599 => Verdict::Retry,
            _ => Verdict::GiveUp,
        },
        Outcome::Timeout | Outcome::Network | Outcome::LeaseExpired => Verdict::Retry,
        Outcome::Rejected => Verdict::GiveUp,
    }
}

/// Delay before the next attempt, after `failed_attempts` failures (1 for the first).
///
/// Follows [`RETRY_DELAYS_MS`], staying on the last entry after that, with "equal jitter":
/// half the delay is fixed and half scales with `jitter` in `[0, 1)`. A destination's
/// `Retry-After` is honoured as a floor, up to [`RETRY_CAP_MS`].
pub fn backoff_ms(failed_attempts: u32, retry_after_ms: Option<i64>, jitter: f64) -> i64 {
    let step = (failed_attempts.saturating_sub(1) as usize).min(RETRY_DELAYS_MS.len() - 1);
    let delay = RETRY_DELAYS_MS[step];
    let jitter = if jitter.is_finite() {
        jitter.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let jittered = delay / 2 + ((delay / 2) as f64 * jitter) as i64;
    jittered.max(retry_after_ms.unwrap_or(0).clamp(0, RETRY_CAP_MS))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(status: u16) -> Outcome {
        Outcome::Http {
            status,
            retry_after_ms: None,
        }
    }

    #[test]
    fn classifies_outcomes() {
        assert_eq!(classify(&http(200)), Verdict::Success);
        assert_eq!(classify(&http(204)), Verdict::Success);
        for s in [408, 425, 429, 500, 502, 503, 599] {
            assert_eq!(classify(&http(s)), Verdict::Retry, "{s}");
        }
        for s in [100, 301, 302, 400, 401, 403, 404, 410, 422] {
            assert_eq!(classify(&http(s)), Verdict::GiveUp, "{s}");
        }
        assert_eq!(classify(&Outcome::Timeout), Verdict::Retry);
        assert_eq!(classify(&Outcome::Network), Verdict::Retry);
        assert_eq!(classify(&Outcome::LeaseExpired), Verdict::Retry);
        assert_eq!(classify(&Outcome::Rejected), Verdict::GiveUp);
    }

    #[test]
    fn backoff_follows_the_schedule_then_stays_on_its_last_step() {
        let full: Vec<_> = (1..=6).map(|n| backoff_ms(n, None, 1.0)).collect();
        assert_eq!(
            full,
            [10_000, 120_000, 1_200_000, 2_400_000, 2_400_000, 2_400_000]
        );
        assert_eq!(backoff_ms(1, None, 0.0), 5_000);
        assert_eq!(backoff_ms(u32::MAX, None, 0.5), 1_800_000);
    }

    #[test]
    fn default_attempts_outlast_a_thirty_minute_outage() {
        let retries = crate::limits::DEFAULT_MAX_ATTEMPTS - 1;
        let span = |jitter| {
            (1..=retries)
                .map(|n| backoff_ms(n, None, jitter))
                .sum::<i64>()
        };
        assert!(span(0.0) > 30 * 60_000, "shortest span: {}", span(0.0));
        assert_eq!(span(1.0), 3_730_000);
    }

    #[test]
    fn backoff_honours_retry_after_up_to_the_cap() {
        assert_eq!(backoff_ms(1, Some(30_000), 0.0), 30_000);
        assert_eq!(backoff_ms(1, Some(1_000), 1.0), 10_000);
        assert_eq!(backoff_ms(1, Some(i64::MAX), 0.0), RETRY_CAP_MS);
        assert_eq!(backoff_ms(1, Some(-5), 0.0), 5_000);
    }

    #[test]
    fn backoff_tolerates_bad_jitter() {
        assert_eq!(backoff_ms(1, None, f64::NAN), 5_000);
        assert_eq!(backoff_ms(1, None, 7.0), 10_000);
    }

    #[test]
    fn labels_are_stable() {
        assert_eq!(http(503).label(), "http_503");
        assert_eq!(Outcome::LeaseExpired.label(), "lease_expired");
    }
}
