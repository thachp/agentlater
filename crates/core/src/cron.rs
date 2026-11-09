//! Five-field cron schedules, evaluated in UTC.
//!
//! `minute hour day-of-month month day-of-week`, each `*`, a number, a range `a-b`, a step
//! `*/n` or `a-b/n`, or a comma list of those. Day of week is 0-7 with 0 and 7 both Sunday.
//! As in Vixie cron, when both day fields are restricted a day matches if *either* does; a
//! day field starting with `*` (like `*/2`) makes both fields apply together instead.

use crate::limits::{DAY_MS, MAX_LANE_BYTES, MINUTE_MS, MIN_CRON_INTERVAL_MS};
use crate::request::{valid_lane, Callback, Method, ScheduleRequest, TaskSpec, ValidationError};
use runlater_egress::DestinationError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

const MAX_SCHEDULE_BYTES: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schedule {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    any_day: bool,
    any_weekday: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CronError {
    TooLong,
    Fields,
    Field(&'static str),
}

impl fmt::Display for CronError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong => write!(f, "schedule is longer than {MAX_SCHEDULE_BYTES} characters"),
            Self::Fields => f.write_str(
                "schedule must have five fields: minute hour day-of-month month day-of-week",
            ),
            Self::Field(name) => write!(f, "schedule has an invalid {name} field"),
        }
    }
}

impl std::error::Error for CronError {}

/// Parses one field into a bitmask over `min..=max`.
fn field(spec: &str, min: u32, max: u32, name: &'static str) -> Result<u64, CronError> {
    let bad = || CronError::Field(name);
    let num = |s: &str, lo: u32, hi: u32| -> Result<u32, CronError> {
        if s.is_empty() || s.len() > 2 || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        let n: u32 = s.parse().map_err(|_| bad())?;
        if n < lo || n > hi {
            return Err(bad());
        }
        Ok(n)
    };
    let mut mask = 0u64;
    for part in spec.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, num(s, 1, max)?),
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            let (a, b) = (num(a, min, max)?, num(b, min, max)?);
            if a > b {
                return Err(bad());
            }
            (a, b)
        } else {
            let a = num(range, min, max)?;
            // `5/15` means from 5 to the end, every 15.
            (a, if part.contains('/') { max } else { a })
        };
        let mut v = lo;
        while v <= hi {
            mask |= 1 << v;
            v += step;
        }
    }
    Ok(mask)
}

impl Schedule {
    pub fn parse(s: &str) -> Result<Self, CronError> {
        if s.len() > MAX_SCHEDULE_BYTES {
            return Err(CronError::TooLong);
        }
        let parts: Vec<&str> = s.split_ascii_whitespace().collect();
        let [m, h, dom, mon, dow] = parts[..] else {
            return Err(CronError::Fields);
        };
        let mut weekdays = field(dow, 0, 7, "day-of-week")?;
        if weekdays & (1 << 7) != 0 {
            weekdays = (weekdays | 1) & 0x7f;
        }
        Ok(Self {
            minutes: field(m, 0, 59, "minute")?,
            hours: field(h, 0, 23, "hour")? as u32,
            days: field(dom, 1, 31, "day-of-month")? as u32,
            months: field(mon, 1, 12, "month")? as u16,
            weekdays: weekdays as u8,
            any_day: dom.starts_with('*'),
            any_weekday: dow.starts_with('*'),
        })
    }

    fn day_matches(&self, day: i64) -> bool {
        let (_, month, dom) = civil(day);
        if self.months & (1 << month) == 0 {
            return false;
        }
        // 1970-01-01 was a Thursday.
        let weekday = (day + 4).rem_euclid(7) as u32;
        let by_dom = self.days & (1 << dom) != 0;
        let by_dow = self.weekdays & (1 << weekday) != 0;
        // A starred field matches every day it allows, so AND leaves the other in charge.
        if self.any_day || self.any_weekday {
            by_dom && by_dow
        } else {
            by_dom || by_dow
        }
    }

    /// The first run strictly after `after_ms`, on a whole minute. `None` if there is none
    /// within about four years (e.g. February 30th).
    pub fn next_after(&self, after_ms: i64) -> Option<i64> {
        let start = after_ms.div_euclid(MINUTE_MS) + 1;
        let mut day = start.div_euclid(1440);
        let mut from_minute = start.rem_euclid(1440);
        for _ in 0..1500 {
            if self.day_matches(day) {
                for minute_of_day in from_minute..1440 {
                    let (h, m) = (minute_of_day / 60, minute_of_day % 60);
                    if self.hours & (1 << h) != 0 && self.minutes & (1 << m) != 0 {
                        return Some(day * DAY_MS + minute_of_day * MINUTE_MS);
                    }
                }
            }
            day += 1;
            from_minute = 0;
        }
        None
    }

    /// Whether runs are always at least an hour apart. With one minute value they are,
    /// even across midnight; with two or more, two runs fall within the same hour.
    pub fn at_most_hourly(&self) -> bool {
        self.minutes.count_ones() == 1
    }
}

/// Cron names: the lane alphabet, starting with a letter or digit so a name is always a
/// plain URL path segment (never `.` or `..`).
pub fn valid_cron_name(name: &str) -> bool {
    valid_lane(name) && name.as_bytes()[0].is_ascii_alphanumeric()
}

/// `create_cron` input exactly as the caller sent it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CronRequest {
    pub name: String,
    pub schedule: String,
    pub url: String,
    #[serde(default)]
    pub method: Method,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub max_attempts: Option<u32>,
    /// Gets a signed POST when the cron starts failing or recovers, or `"inbox"`.
    #[serde(default)]
    pub alert_url: Option<String>,
    /// Where each run reports when it finishes, as `schedule_http`'s `callback_url`.
    #[serde(default)]
    pub callback_url: Option<String>,
    /// Keep each run's response, as `schedule_http`'s `capture`.
    #[serde(default)]
    pub capture: bool,
    /// Which runs report to `callback_url`. `failures` needs `callback_url`.
    #[serde(default)]
    pub report: Option<Report>,
}

/// Which of a cron's runs report to its `callback_url`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Report {
    /// Every run that finishes.
    #[default]
    Always,
    /// Only runs that fail, so a healthy cron stays quiet.
    Failures,
}

impl Report {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Failures => "failures",
        }
    }
}

/// How a cron's runs have been going.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// No run has finished yet.
    #[default]
    Pending,
    /// The last run that finished succeeded.
    Healthy,
    /// A run failed, or one could not be started.
    Failing,
}

/// What news about one slot does to a cron's health.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HealthChange {
    pub health: Health,
    /// The alert to send, if the cron just turned failing or recovered.
    pub alert: Option<&'static str>,
}

impl Health {
    /// The health after news that the run for `slot` succeeded or failed (or could not start),
    /// given the slot whose news last set it. Older news changes nothing (`None`).
    pub fn after(self, set_by: Option<i64>, slot: i64, succeeded: bool) -> Option<HealthChange> {
        if set_by.is_some_and(|t| t > slot) {
            return None;
        }
        let (health, alert) = match (succeeded, self) {
            (true, Health::Failing) => (Health::Healthy, Some("cron.recovered")),
            (true, _) => (Health::Healthy, None),
            (false, Health::Failing) => (Health::Failing, None),
            (false, _) => (Health::Failing, Some("cron.failing")),
        };
        Some(HealthChange { health, alert })
    }
}

/// A validated cron, ready to store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronSpec {
    pub name: String,
    pub schedule: Schedule,
    pub schedule_text: String,
    /// The request each run makes. `run_at_ms` is meaningless here.
    /// Its `callback` and `capture` apply to every run.
    pub request: TaskSpec,
    pub alert: Option<Callback>,
    pub report: Report,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CronRequestError {
    Name,
    Schedule(CronError),
    TooFrequent,
    Never,
    Alert(DestinationError),
    ReportWithoutCallback,
    Request(ValidationError),
}

impl CronRequestError {
    pub fn field(&self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Schedule(_) | Self::TooFrequent | Self::Never => "schedule",
            Self::Alert(_) => "alert_url",
            Self::ReportWithoutCallback => "report",
            Self::Request(e) => e.field(),
        }
    }
}

impl fmt::Display for CronRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name => write!(
                f,
                "name must be 1-{MAX_LANE_BYTES} characters of A-Z a-z 0-9 _ - . :, \
                 starting with a letter or digit"
            ),
            Self::Schedule(e) => e.fmt(f),
            Self::TooFrequent => write!(
                f,
                "schedule runs more often than every {} minutes",
                MIN_CRON_INTERVAL_MS / MINUTE_MS
            ),
            Self::Never => f.write_str("schedule never runs"),
            Self::Alert(e) => write!(f, "alert_url: {e}"),
            Self::ReportWithoutCallback => {
                f.write_str("report \"failures\" needs a callback_url to report to")
            }
            Self::Request(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for CronRequestError {}

impl CronRequest {
    pub fn validate(self, now_ms: i64) -> Result<CronSpec, CronRequestError> {
        if !valid_cron_name(&self.name) {
            return Err(CronRequestError::Name);
        }
        let schedule = Schedule::parse(&self.schedule).map_err(CronRequestError::Schedule)?;
        if !schedule.at_most_hourly() {
            return Err(CronRequestError::TooFrequent);
        }
        if schedule.next_after(now_ms).is_none() {
            return Err(CronRequestError::Never);
        }
        let alert = self
            .alert_url
            .as_deref()
            .map(Callback::parse)
            .transpose()
            .map_err(CronRequestError::Alert)?;
        // "always" is the default, so stating it without a callback_url is harmless.
        if self.report == Some(Report::Failures) && self.callback_url.is_none() {
            return Err(CronRequestError::ReportWithoutCallback);
        }
        let request = ScheduleRequest {
            url: self.url,
            method: self.method,
            headers: self.headers,
            body: self.body,
            max_attempts: self.max_attempts,
            callback_url: self.callback_url,
            capture: self.capture,
            ..Default::default()
        }
        .validate(now_ms)
        .map_err(CronRequestError::Request)?;
        Ok(CronSpec {
            name: self.name,
            schedule_text: self
                .schedule
                .split_ascii_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            schedule,
            request,
            alert,
            report: self.report.unwrap_or_default(),
        })
    }
}

/// Days since the unix epoch to (year, month 1-12, day 1-31). Howard Hinnant's algorithm.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::HOUR_MS;

    /// 2026-01-01T00:00:00Z, a Thursday.
    const T0: i64 = 1_767_225_600_000;

    #[test]
    fn health_turns_failing_and_recovers_with_one_alert_each() {
        let fail = Health::Pending.after(None, 10, false).unwrap();
        assert_eq!(
            (fail.health, fail.alert),
            (Health::Failing, Some("cron.failing"))
        );
        let again = Health::Failing.after(Some(10), 20, false).unwrap();
        assert_eq!((again.health, again.alert), (Health::Failing, None));
        let back = Health::Failing.after(Some(20), 30, true).unwrap();
        assert_eq!(
            (back.health, back.alert),
            (Health::Healthy, Some("cron.recovered"))
        );
        let fine = Health::Pending.after(None, 10, true).unwrap();
        assert_eq!((fine.health, fine.alert), (Health::Healthy, None));
    }

    #[test]
    fn older_news_does_not_change_health() {
        assert_eq!(Health::Failing.after(Some(20), 10, true), None);
        assert!(Health::Healthy.after(Some(20), 20, false).is_some());
    }

    fn next(s: &str, after: i64) -> i64 {
        Schedule::parse(s).unwrap().next_after(after).unwrap()
    }

    #[test]
    fn converts_days_to_dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(T0 / DAY_MS), (2026, 1, 1));
        assert_eq!(civil(T0 / DAY_MS + 58), (2026, 2, 28));
        assert_eq!(civil(T0 / DAY_MS + 59), (2026, 3, 1));
        assert_eq!(civil(11_016), (2000, 2, 29));
    }

    #[test]
    fn finds_the_next_run() {
        assert_eq!(next("0 6 * * *", T0), T0 + 6 * HOUR_MS);
        assert_eq!(
            next("0 6 * * *", T0 + 6 * HOUR_MS),
            T0 + DAY_MS + 6 * HOUR_MS
        );
        assert_eq!(next("* * * * *", T0 + 1), T0 + MINUTE_MS);
        assert_eq!(next("30 */4 * * *", T0), T0 + 30 * MINUTE_MS);
        assert_eq!(
            next("30 */4 * * *", T0 + 30 * MINUTE_MS),
            T0 + 4 * HOUR_MS + 30 * MINUTE_MS
        );
        assert_eq!(next("15/20 * * * *", T0), T0 + 15 * MINUTE_MS);
        assert_eq!(
            next("15/20 * * * *", T0 + 50 * MINUTE_MS),
            T0 + 55 * MINUTE_MS
        );
        // Monday 2026-01-05.
        assert_eq!(next("0 9 * * 1", T0), T0 + 4 * DAY_MS + 9 * HOUR_MS);
        assert_eq!(next("0 9 * * 1-5", T0), T0 + 9 * HOUR_MS);
        // Sunday as 0 and as 7: 2026-01-04.
        assert_eq!(next("0 0 * * 7", T0), T0 + 3 * DAY_MS);
        assert_eq!(next("0 0 * * 0", T0), T0 + 3 * DAY_MS);
        // March 1st.
        assert_eq!(next("0 0 1 3 *", T0), T0 + 59 * DAY_MS);
        // Either day field: the 15th, or any Monday, whichever comes first.
        assert_eq!(next("0 0 15 * 1", T0), T0 + 4 * DAY_MS);
        assert_eq!(Schedule::parse("0 0 30 2 *").unwrap().next_after(T0), None);
    }

    #[test]
    fn rejects_bad_schedules() {
        for (s, e) in [
            ("* * * *", CronError::Fields),
            ("* * * * * *", CronError::Fields),
            ("60 * * * *", CronError::Field("minute")),
            ("* 24 * * *", CronError::Field("hour")),
            ("* * 0 * *", CronError::Field("day-of-month")),
            ("* * * 13 *", CronError::Field("month")),
            ("* * * * 8", CronError::Field("day-of-week")),
            ("*/0 * * * *", CronError::Field("minute")),
            ("*/61 * * * *", CronError::Field("minute")),
            ("5-1 * * * *", CronError::Field("minute")),
            ("a * * * *", CronError::Field("minute")),
            ("1,,2 * * * *", CronError::Field("minute")),
            ("@daily * * * *", CronError::Field("minute")),
        ] {
            assert_eq!(Schedule::parse(s), Err(e), "{s:?}");
        }
        assert!(Schedule::parse(" 0  6 * * * ").is_ok());
    }

    #[test]
    fn knows_when_runs_are_at_most_hourly() {
        let hourly = |s: &str| Schedule::parse(s).unwrap().at_most_hourly();
        assert!(hourly("0 * * * *"));
        assert!(hourly("59 23 31 12 *"));
        assert!(hourly("0 9,17 * * 1-5"));
        assert!(!hourly("*/30 * * * *"));
        assert!(!hourly("0,30 9 * * *"));
        assert!(!hourly("0-1 3 * * *"));
    }

    #[test]
    fn a_starred_day_field_does_not_widen_the_other() {
        // Vixie cron: when a day field starts with *, both must match.
        // An odd day *and* a Monday: 2026-01-05, then 2026-01-19.
        assert_eq!(next("0 9 */2 * 1", T0), T0 + 4 * DAY_MS + 9 * HOUR_MS);
        assert_eq!(
            next("0 9 */2 * 1", T0 + 5 * DAY_MS),
            T0 + 18 * DAY_MS + 9 * HOUR_MS
        );
    }

    #[test]
    fn refuses_very_long_schedules() {
        let long = format!("{} * * * *", vec!["0"; 100].join(","));
        assert_eq!(Schedule::parse(&long), Err(CronError::TooLong));
    }

    fn cron(schedule: &str) -> CronRequest {
        CronRequest {
            name: "daily-backup".into(),
            schedule: schedule.into(),
            url: "https://myapp.example.com/api/backup".into(),
            ..Default::default()
        }
    }

    #[test]
    fn validates_cron_requests() {
        let spec = cron("0  6 * * *").validate(T0).unwrap();
        assert_eq!(spec.schedule_text, "0 6 * * *");
        assert_eq!(spec.request.method, Method::Post);
        assert_eq!(spec.alert, None);

        let field = |r: CronRequest| r.validate(T0).unwrap_err().field();
        assert_eq!(
            field(CronRequest {
                name: "a b".into(),
                ..cron("0 6 * * *")
            }),
            "name"
        );
        assert_eq!(field(cron("0 6 * *")), "schedule");
        for bad in ["..", ".", "-x", ":x"] {
            assert_eq!(
                field(CronRequest {
                    name: bad.into(),
                    ..cron("0 6 * * *")
                }),
                "name"
            );
        }
        assert_eq!(
            cron("*/30 * * * *").validate(T0).unwrap_err(),
            CronRequestError::TooFrequent
        );
        assert!(cron("0 * * * *").validate(T0).is_ok());
        assert_eq!(
            cron("0 0 30 2 *").validate(T0).unwrap_err(),
            CronRequestError::Never
        );
        assert_eq!(
            field(CronRequest {
                alert_url: Some("http://x.example.com/".into()),
                ..cron("0 6 * * *")
            }),
            "alert_url"
        );
        assert_eq!(
            field(CronRequest {
                url: "https://localhost/".into(),
                ..cron("0 6 * * *")
            }),
            "url"
        );
        assert!(serde_json::from_str::<CronRequest>(
            r#"{"name":"a","schedule":"0 6 * * *","url":"https://a.example.com/","run_at_ms":1}"#
        )
        .is_err());
    }

    #[test]
    fn runs_can_report_and_capture() {
        let base = || CronRequest {
            name: "deploy-health".into(),
            schedule: "0 7 * * *".into(),
            url: "https://a.example.com/status".into(),
            ..Default::default()
        };
        let quiet = base().validate(T0).unwrap();
        assert_eq!(
            (quiet.request.callback, quiet.request.capture),
            (None, false)
        );
        assert_eq!(quiet.report, Report::Always);

        let spec = CronRequest {
            callback_url: Some("inbox".into()),
            capture: true,
            report: Some(Report::Failures),
            ..base()
        }
        .validate(T0)
        .unwrap();
        assert_eq!(spec.request.callback, Some(Callback::Inbox));
        assert!(spec.request.capture);
        assert_eq!(spec.report, Report::Failures);

        assert!(CronRequest {
            report: Some(Report::Always),
            ..base()
        }
        .validate(T0)
        .is_ok());
        let e = CronRequest {
            report: Some(Report::Failures),
            ..base()
        }
        .validate(T0)
        .unwrap_err();
        assert_eq!(e, CronRequestError::ReportWithoutCallback);
        assert_eq!(e.field(), "report");
        let e = CronRequest {
            callback_url: Some("https://localhost/x".into()),
            ..base()
        }
        .validate(T0)
        .unwrap_err();
        assert_eq!(e.field(), "callback_url");
        assert!(serde_json::from_str::<CronRequest>(
            r#"{"name":"a","schedule":"0 7 * * *","url":"https://a.example.com/","report":"sometimes"}"#
        )
        .is_err());
    }
}
