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

/** The free trial and the per-task attempt cap, as the landing page states them (pinned with LIMITS below). */
export const TRIAL_ATTEMPTS = "1,000";
export const TRIAL_DAYS = 7;
export const MAX_ATTEMPTS = 5;

export const LIMITS: [string, string][] = [
  ["Free trial", `${TRIAL_ATTEMPTS} delivery attempts over ${TRIAL_DAYS} days, per project`],
  ["More attempts", "50,000 for $5, valid 12 months; leftovers carry over"],
  ["Projects", "One per agent host"],
  ["Schedule ahead", "Up to 7 days, and before the project's attempts expire"],
  ["Request body", "64 KiB"],
  ["Headers", "32 headers, 8 KiB in total"],
  ["Attempts per task", `1–${MAX_ATTEMPTS} (default ${MAX_ATTEMPTS})`],
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
