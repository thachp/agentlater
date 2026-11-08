//! The API's routes: one table that both the entry Worker (which scope may call it) and the
//! ledger (what to do) read, so a route can't be dispatched without a scope or the reverse.

use crate::assertion::Scope;

/// The two methods the API answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verb {
    Get,
    Post,
}

/// A route, with the path segments it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route<'p> {
    CreateProject,
    GetProject,
    AddCredit,
    Schedule,
    ListTasks,
    GetTask(&'p str),
    CancelTask(&'p str),
    RedriveTask(&'p str),
    Watch,
    PutCron,
    ListCrons,
    GetCron(&'p str),
    DeleteCron(&'p str),
    ReadInbox,
    AckInbox,
    SetWake,
    DeleteWake,
    PutSecret,
    ListSecrets,
    DeleteSecret(&'p str),
    PutHook,
    ListHooks,
    ReceiveHook,
    DeleteHook(&'p str),
    UpdateHook(&'p str),
    /// The queue consumer starting an attempt. Ledger-only: never reachable from outside.
    Claim,
    /// The queue consumer recording an attempt. Ledger-only.
    Finish,
}

impl<'p> Route<'p> {
    pub fn parse(verb: Verb, path: &[&'p str]) -> Option<Route<'p>> {
        use Verb::{Get, Post};
        Some(match (verb, path) {
            (Post, ["v1", "projects"]) => Route::CreateProject,
            (Get, ["v1", "projects", "self"]) => Route::GetProject,
            (Post, ["v1", "projects", "self", "credits"]) => Route::AddCredit,
            (Post, ["v1", "tasks"]) => Route::Schedule,
            (Get, ["v1", "tasks"]) => Route::ListTasks,
            (Get, ["v1", "tasks", id]) => Route::GetTask(id),
            (Post, ["v1", "tasks", id, "cancel"]) => Route::CancelTask(id),
            (Post, ["v1", "tasks", id, "redrive"]) => Route::RedriveTask(id),
            (Post, ["v1", "watches"]) => Route::Watch,
            (Post, ["v1", "crons"]) => Route::PutCron,
            (Get, ["v1", "crons"]) => Route::ListCrons,
            (Get, ["v1", "crons", name]) => Route::GetCron(name),
            (Post, ["v1", "crons", name, "delete"]) => Route::DeleteCron(name),
            (Post, ["v1", "inbox", "read"]) => Route::ReadInbox,
            (Post, ["v1", "inbox", "ack"]) => Route::AckInbox,
            (Post, ["v1", "wake"]) => Route::SetWake,
            (Post, ["v1", "wake", "delete"]) => Route::DeleteWake,
            (Post, ["v1", "secrets"]) => Route::PutSecret,
            (Get, ["v1", "secrets"]) => Route::ListSecrets,
            (Post, ["v1", "secrets", name, "delete"]) => Route::DeleteSecret(name),
            (Post, ["v1", "hooks"]) => Route::PutHook,
            (Get, ["v1", "hooks"]) => Route::ListHooks,
            (Post, ["v1", "hooks", "_receive"]) => Route::ReceiveHook,
            (Post, ["v1", "hooks", name, "delete"]) => Route::DeleteHook(name),
            // After _receive: a hook name never starts with "_".
            (Post, ["v1", "hooks", name]) => Route::UpdateHook(name),
            (Post, ["internal", "claim"]) => Route::Claim,
            (Post, ["internal", "finish"]) => Route::Finish,
            _ => return None,
        })
    }

    /// The scope a gateway assertion needs to call it; None for the ledger-only routes,
    /// which the entry Worker never forwards.
    pub fn scope(self) -> Option<Scope> {
        Some(match self {
            Route::CreateProject => Scope::Project,
            Route::AddCredit => Scope::Billing,
            Route::Schedule
            | Route::RedriveTask(_)
            | Route::Watch
            | Route::PutCron
            | Route::SetWake
            | Route::PutHook
            | Route::UpdateHook(_) => Scope::Schedule,
            Route::CancelTask(_)
            | Route::DeleteCron(_)
            | Route::DeleteWake
            | Route::DeleteHook(_) => Scope::Cancel,
            Route::PutSecret | Route::DeleteSecret(_) => Scope::Secrets,
            Route::ReceiveHook => Scope::HooksReceive,
            // Acking is part of reading: it moves the project's one cursor, nothing else.
            // Secret and hook lists hold names, hosts, settings and counts, never values or URLs.
            Route::GetProject
            | Route::ListTasks
            | Route::GetTask(_)
            | Route::ListCrons
            | Route::GetCron(_)
            | Route::ReadInbox
            | Route::AckInbox
            | Route::ListSecrets
            | Route::ListHooks => Scope::Read,
            Route::Claim | Route::Finish => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Verb::{Get, Post};

    fn scope(verb: Verb, path: &str) -> Option<Scope> {
        let path: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        Route::parse(verb, &path)?.scope()
    }

    #[test]
    fn every_public_route_needs_its_scope() {
        let table = [
            (Post, "/v1/projects", Scope::Project),
            (Get, "/v1/projects/self", Scope::Read),
            (Post, "/v1/projects/self/credits", Scope::Billing),
            (Post, "/v1/tasks", Scope::Schedule),
            (Get, "/v1/tasks", Scope::Read),
            (Get, "/v1/tasks/t1", Scope::Read),
            (Post, "/v1/tasks/t1/cancel", Scope::Cancel),
            (Post, "/v1/tasks/t1/redrive", Scope::Schedule),
            (Post, "/v1/watches", Scope::Schedule),
            (Post, "/v1/crons", Scope::Schedule),
            (Get, "/v1/crons", Scope::Read),
            (Get, "/v1/crons/c", Scope::Read),
            (Post, "/v1/crons/c/delete", Scope::Cancel),
            (Post, "/v1/inbox/read", Scope::Read),
            (Post, "/v1/inbox/ack", Scope::Read),
            (Post, "/v1/wake", Scope::Schedule),
            (Post, "/v1/wake/delete", Scope::Cancel),
            (Post, "/v1/secrets", Scope::Secrets),
            (Get, "/v1/secrets", Scope::Read),
            (Post, "/v1/secrets/GH/delete", Scope::Secrets),
            (Post, "/v1/hooks", Scope::Schedule),
            (Get, "/v1/hooks", Scope::Read),
            (Post, "/v1/hooks/_receive", Scope::HooksReceive),
            (Post, "/v1/hooks/h/delete", Scope::Cancel),
            (Post, "/v1/hooks/h", Scope::Schedule),
        ];
        for (verb, path, expected) in table {
            assert_eq!(scope(verb, path), Some(expected), "{verb:?} {path}");
        }
    }

    #[test]
    fn ledger_only_and_unknown_routes_have_no_scope() {
        assert_eq!(
            Route::parse(Post, &["internal", "claim"]),
            Some(Route::Claim)
        );
        assert_eq!(scope(Post, "/internal/claim"), None);
        assert_eq!(scope(Post, "/internal/finish"), None);
        assert_eq!(scope(Get, "/v1/hooks/h"), None);
        assert_eq!(scope(Post, "/v1/tasks/t1"), None);
        assert_eq!(scope(Get, "/v1/nothing"), None);
    }

    #[test]
    fn receive_is_not_a_hook_named_underscore_receive() {
        assert_eq!(
            Route::parse(Post, &["v1", "hooks", "_receive"]),
            Some(Route::ReceiveHook)
        );
        assert_eq!(
            Route::parse(Post, &["v1", "hooks", "gh"]),
            Some(Route::UpdateHook("gh"))
        );
    }

    /// Every operation in the API's contract with the scope it says, and no route it leaves out.
    #[test]
    fn matches_the_contract() {
        let doc = include_str!("../../../packages/contracts/openapi.yaml");
        let (mut path, mut verb) = ("", None);
        let mut seen = std::collections::HashSet::new();
        for line in doc.lines() {
            if let Some(p) = line.strip_prefix("  /").and_then(|p| p.strip_suffix(':')) {
                path = p;
            } else if let Some(v) = line.strip_prefix("    ").and_then(|v| v.strip_suffix(':')) {
                verb = match v {
                    "get" => Some(Get),
                    "post" => Some(Post),
                    _ => None,
                };
            } else if let Some(want) = line.strip_prefix("      x-scope: ") {
                let verb = verb.expect("x-scope under a method");
                // Fill each {param} with a sample segment.
                let segments: Vec<&str> = path
                    .split('/')
                    .map(|s| if s.starts_with('{') { "p" } else { s })
                    .collect();
                let route = Route::parse(verb, &segments);
                assert_eq!(
                    route.and_then(Route::scope),
                    Some(Scope::parse(want).expect("a known scope")),
                    "{verb:?} /{path}"
                );
                assert!(seen.insert((verb, path)), "{verb:?} /{path} listed twice");
            }
        }
        // Every route with a scope is in the contract, once.
        assert_eq!(seen.len(), 25);
    }
}
