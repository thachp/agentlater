// Facts the site states. Keep in step with docs/spec/PRODUCT.md and crates/core/src/limits.rs.

/** Where agents connect once the gateway is deployed (docs/DEPLOY.md, phase 4). */
export const GATEWAY_URL = "https://gateway.runlater.io";
export const REPO_URL = "https://github.com/thachp/runlater";

/** Who operates the hosted service, named in the Terms and Privacy Policy. */
export const OPERATOR = "OM.Network, LLC";
export const CONTACT_EMAIL = "support@runlater.io";
export const LEGAL_UPDATED = "November 6, 2026";

export const LIMITS: [string, string][] = [
  ["Free trial", "100 delivery attempts over 7 days, per project"],
  ["More attempts", "50,000 for $5, valid 12 months; leftovers carry over"],
  ["Projects", "One per agent host"],
  ["Schedule ahead", "Up to 7 days, and before the project's attempts expire"],
  ["Request body", "64 KiB"],
  ["Headers", "32 headers, 8 KiB in total"],
  ["Attempts per task", "1–5 (default 5)"],
  ["Attempt timeout", "10 seconds; redirects are not followed"],
  ["Tasks held", "1,000 per project (20,000 once paid), finished ones included until purged"],
  ["Retention", "Tasks and attempts are kept 30 days after they finish"],
  ["Crons", "10 per project, each running at most once an hour"],
  ["Watches", "A poll every 5 minutes to 24 hours, for up to 7 days"],
  ["Captured responses", "First 16 KiB and 4 headers, kept 7 days after the task finishes"],
  ["Inbox", "10,000 events per project; unread kept 30 days, acked 7 days"],
  ["Wake", "At most one per 1 minute to 24 hours (default 15 minutes); off after 3 failures in a row"],
  ["Secrets", "50 per project, 4 KiB each, each bound to at most 20 hosts"],
  ["Hooks", "20 per project; 60 requests a minute each; 1,000 events a day (10,000 once paid), 100 per unverified hook; 256 KiB per request"],
];
