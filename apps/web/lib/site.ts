// Facts the site states. Keep in step with docs/spec/PRODUCT.md and crates/core/src/limits.rs.

/** Where agents connect once the gateway is deployed (docs/DEPLOY.md, phase 4). */
export const GATEWAY_URL = "https://gateway.agentlater.io";
export const REPO_URL = "https://github.com/thachp/agentlater";

/** Who operates the hosted service, named in the Terms and Privacy Policy. */
export const OPERATOR = "OM.Network, LLC";
export const CONTACT_EMAIL = "support@agentlater.io";
export const LEGAL_UPDATED = "November 6, 2026";

/** The wait before each retry: half to all of each delay in RETRY_DELAYS_MS (crates/core/src/limits.rs). */
export const RETRIES: [string, string][] = [
  ["after attempt 1", "5–10 s"],
  ["after attempt 2", "1–2 min"],
  ["after attempt 3", "10–20 min"],
  ["after attempt 4", "20–40 min"],
];

export const LIMITS: [string, string][] = [
  ["Free trial", "1,000 delivery attempts over 7 days, per project"],
  ["More attempts", "50,000 for $5, valid 12 months; leftovers carry over"],
  ["Projects", "One per agent host"],
  ["Schedule ahead", "Up to 7 days, and before the project's attempts expire"],
  ["Request body", "64 KiB"],
  ["Headers", "32 headers, 8 KiB in total"],
  ["Attempts per task", "1–5 (default 5)"],
  ["Attempt timeout", "10 seconds; redirects are not followed"],
  ["Tasks held", "1,000 per project (20,000 once paid), finished ones included until purged"],
  ["Retention", "Tasks and attempts are kept 30 days after they finish"],
  ["Signing secret rotation", "Overlap of 0 seconds to 7 days (default 24 hours)"],
  ["Crons", "10 per project, each running at most once an hour"],
  ["Watches", "A poll every 5 minutes to 24 hours, for up to 7 days"],
  ["Captured responses", "First 16 KiB and 4 headers, kept 7 days after the task finishes"],
  ["Inbox", "10,000 events per project; unread kept 30 days, acked 7 days"],
  ["Wake", "At most one per 1 minute to 24 hours (default 15 minutes); off after 3 failures in a row"],
  ["Secrets", "50 per project, 4 KiB each, each bound to at most 20 hosts"],
  ["Hooks", "20 per project; 60 requests a minute each; 1,000 events a day (10,000 once paid), 100 per unverified hook; 256 KiB per request"],
];

// ---- A2A (app/docs/a2a). Each is checked against apps/gateway/src by test/snippets.test.ts.

/** The gateway the A2A page's samples call: one that serves the Agent Card and /a2a (its A2A_ENABLED is "true"). */
export const A2A_GATEWAY_URL = "https://gateway-staging.agentlater.io";

/** A delivery status, the A2A task state it reads as, and what it means. */
export const A2A_STATES: [string, string, string][] = [
  ["scheduled", "TASK_STATE_SUBMITTED", "Waiting for its time."],
  ["delivering", "TASK_STATE_WORKING", "An attempt is being sent."],
  ["retrying", "TASK_STATE_WORKING", "The last attempt failed in a way that is retried; the next one is scheduled."],
  ["succeeded", "TASK_STATE_COMPLETED", "The destination answered 2xx."],
  ["failed", "TASK_STATE_FAILED", "The destination refused it for good, or no attempts are left."],
  ["canceled", "TASK_STATE_CANCELED", "Canceled before it was delivered."],
];

/** The fields of the one data part SendMessage takes. */
export const A2A_PAYLOAD: [string, string][] = [
  ["url", "Required. An https:// URL on a public domain name."],
  ["method", "GET, POST, PUT, PATCH or DELETE. Default POST."],
  ["headers", "Up to 32 headers, 8 KiB in total. Host, Content-Length and the RunLater-*, CF-* and X-Forwarded-* families are not allowed."],
  ["body", "The request body as text, at most 64 KiB. Not with GET."],
  ["run_at_ms", "When to send, unix milliseconds, at most 7 days ahead. Not with delay."],
  ["delay", "Send after this long: a whole number and s, m, h or d, such as \"15m\". At most 7d. Not with run_at_ms."],
  ["idempotency_key", "1 to 128 of A-Z a-z 0-9 _ -. Without one, the message's messageId is the key."],
  ["max_attempts", "1 to 5. Default 5."],
  ["capture", "true keeps the destination's last response in the task's artifact, for 7 days after the task finishes."],
];

export const A2A_LIMITS: [string, string][] = [
  ["Requests to /a2a", "60 a minute from one IP address, all agents and operations together"],
  ["Capability calls", "300 a minute per host, counted with the host's calls over Agent Auth"],
  ["Token", "Lives 60 seconds and is accepted once"],
  ["Agent", "Revoked 7 days after it is registered; the host key stays valid"],
  ["Request size", "192 KiB"],
  ["ListTasks page", "1 to 100 tasks (default 50)"],
];
