//! AgentLater task API: the authoritative task ledger, scheduling and delivery.
//!
//! Reachable only through a service binding from `apps/gateway`; it has no public route.
//! Everything except `worker_entry` is plain Rust and tested natively.

pub mod assertion;
pub mod db;
pub mod http;
pub mod ledger;
pub mod sealing;

#[cfg(target_arch = "wasm32")]
mod worker_entry;

/// Response body for `GET /health`.
pub fn health_body() -> serde_json::Value {
    serde_json::json!({ "service": "runlater-api", "status": "ok" })
}

#[cfg(test)]
mod tests {
    #[test]
    fn health_reports_ok() {
        assert_eq!(super::health_body()["status"], "ok");
    }
}
