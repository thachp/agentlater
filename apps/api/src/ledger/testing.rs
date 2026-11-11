//! Helpers the ledger's test modules share. `ledger.rs` declares this file `#[cfg(test)]`.

use super::*;
use crate::db::sqlite::Memory;

pub(super) const T0: i64 = 1_767_225_600_000;
pub(super) const P: &str = "proj_1";

pub(super) fn http(status: u16) -> Outcome {
    Outcome::Http {
        status,
        retry_after_ms: None,
    }
}

pub(super) fn req(url: &str) -> ScheduleRequest {
    ScheduleRequest {
        url: url.into(),
        ..Default::default()
    }
}

pub(super) fn setup(db: &Memory) -> Ledger<'_, Memory> {
    let l = Ledger::open(db)
        .unwrap()
        .with_sealer(Some(secrets::test_sealer()));
    l.create_project(P, "secret", T0).unwrap();
    l
}

pub(super) fn deliver(l: &Ledger<'_, Memory>, d: &Dispatch, now: i64) -> Delivery {
    match l.claim(&d.task_id, d.generation, now).unwrap() {
        Claimed::Deliver(d) => *d,
        other => panic!("expected delivery, got {other:?}"),
    }
}

pub(super) fn daily(name: &str) -> CronRequest {
    CronRequest {
        name: name.into(),
        schedule: "0 6 * * *".into(),
        url: "https://a.example.com/backup".into(),
        max_attempts: Some(1),
        alert_url: Some("https://alerts.example.com/hook".into()),
        ..Default::default()
    }
}

pub(super) const SIX: i64 = T0 + 6 * 3_600_000;
pub(super) const DAY: i64 = 86_400_000;

pub(super) fn alerts(db: &Memory, name: &str) -> Vec<serde_json::Value> {
    let mut stmt =
        db.0.prepare("SELECT body FROM tasks WHERE alert_for = ? ORDER BY seq")
            .unwrap();
    stmt.query_map([name], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|b| serde_json::from_str(&b.unwrap()).unwrap())
        .collect()
}

pub(super) fn watching(extra: serde_json::Value) -> WatchRequest {
    let mut v = serde_json::json!({
        "url": "https://api.example.com/jobs/1",
        "headers": { "Authorization": "Bearer secret-token" },
        "every": "5m",
        "until": { "path": "/status", "in": ["succeeded", "failed"] },
        "deadline": "1h",
    });
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    serde_json::from_value(v).unwrap()
}

pub(super) fn captured(extra: serde_json::Value) -> ScheduleRequest {
    let mut r = serde_json::json!({ "url": "https://a.example.com/", "capture": true });
    r.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    serde_json::from_value(r).unwrap()
}

/// Publishes what is due at `now`, claims `id` and finishes its attempt with this result.
pub(super) fn drive(
    l: &Ledger<'_, Memory>,
    id: &str,
    outcome: Outcome,
    read: Option<ResponseRead<'_>>,
    now: i64,
) -> (Delivery, Option<Status>) {
    let d = l
        .publish_due(P, now)
        .unwrap()
        .into_iter()
        .find(|d| d.task_id == id)
        .expect("task is due");
    let a = deliver(l, &d, now);
    let status = l
        .finish_response(id, a.fence, outcome, read, now, 0.0)
        .unwrap();
    (a, status)
}

/// Delivers `id` now and finishes it with this response.
pub(super) fn answer(
    l: &Ledger<'_, Memory>,
    id: &str,
    status: u16,
    body: &str,
    headers: &[(&str, &str)],
    now: i64,
) -> Option<Status> {
    let headers: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let read = ResponseRead {
        body: Some(PollBody {
            text: body,
            truncated: false,
        }),
        headers: &headers,
    };
    drive(l, id, http(status), Some(read), now).1
}

pub(super) fn inbox(l: &Ledger<'_, Memory>) -> InboxPage {
    l.read_inbox(None, None, false).unwrap()
}

pub(super) fn waking(extra: serde_json::Value) -> WakeRequest {
    let mut r = serde_json::json!({
        "url": "https://agents.example.com/trigger",
        "headers": { "Authorization": "Bearer t0ken" },
    });
    r.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    serde_json::from_value(r).unwrap()
}

pub(super) fn wake_tasks(db: &Memory) -> i64 {
    db.0.query_row("SELECT COUNT(*) FROM tasks WHERE wake_call = 1", [], |r| {
        r.get(0)
    })
    .unwrap()
}

pub(super) fn hosts(h: &[&str]) -> Vec<String> {
    h.iter().map(|s| s.to_string()).collect()
}

pub(super) fn put_secret(
    l: &Ledger<'_, Memory>,
    name: &str,
    value_bytes: usize,
    allowed_hosts: &[&str],
    now: i64,
) -> Result<(SecretView, bool), ApiError> {
    let spec = runlater_core::SecretSpec {
        name: name.into(),
        value: "x".repeat(value_bytes),
        allowed_hosts: hosts(allowed_hosts),
    };
    l.put_secret(&spec, now)
}

/// What `put_secret` stored: the test sealer's nonce is fixed.
pub(super) fn sealed(name: &str, value_bytes: usize) -> SealedSecret {
    let sealer = secrets::test_sealer();
    let nonce = (sealer.nonce)().unwrap();
    SealedSecret {
        name: name.into(),
        sealed: sealer.keys.seal(P, name, &"x".repeat(value_bytes), nonce),
    }
}
