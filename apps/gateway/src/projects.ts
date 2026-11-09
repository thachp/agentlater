// Maps an owner to its AgentLater project: an Agent Auth host id, or `mcp:<id>` for a hosted MCP
// connection. One project per owner, never reassigned.

import { ApiClient, ApiError } from "./api";
import { billingEnabled, paymentLink, type BillingEnv } from "./billing";
import { ArgumentError, execute } from "./capabilities";

/** Errors that buying attempts fixes; the agent is told how. */
const OUT_OF_ATTEMPTS = new Set(["budget_exhausted", "trial_expired"]);
/** TOOL_RATE's limit in wrangler.jsonc, for the message. */
export const TOOL_CALLS_PER_MINUTE = 300;

export async function findProject(db: D1Database, owner: string): Promise<string | null> {
  const row = await db
    .prepare("SELECT project_id FROM runlater_project WHERE host_id = ?")
    .bind(owner)
    .first<{ project_id: string }>();
  return row?.project_id ?? null;
}

const DAY_MS = 24 * 3600 * 1000;

/** Refuses a new trial once `limit` have started in the last 24 hours. Bounds what free identities can cost. */
async function checkTrialCapacity(db: D1Database, limit: number, now: number): Promise<void> {
  const row = await db
    .prepare("SELECT COUNT(*) AS started FROM runlater_project WHERE created_at > ?")
    .bind(now - DAY_MS)
    .first<{ started: number }>();
  if ((row?.started ?? 0) >= limit) {
    throw new ApiError(429, {
      error: { code: "trials_paused", message: "new free trials are full for today; try create_project again later" },
    });
  }
}

/**
 * Returns the owner's project id, allocating one on first call. Safe to repeat, and an owner with
 * a project is never refused. Concurrent calls can pass `trialsPerDay` by a few; it is a budget, not a contract.
 */
export async function ensureProject(db: D1Database, owner: string, trialsPerDay?: number, now = Date.now()): Promise<string> {
  const found = await findProject(db, owner);
  if (found) return found;
  if (trialsPerDay !== undefined) await checkTrialCapacity(db, trialsPerDay, now);
  const id = `prj_${[...crypto.getRandomValues(new Uint8Array(12))].map((b) => b.toString(16).padStart(2, "0")).join("")}`;
  await db
    .prepare("INSERT OR IGNORE INTO runlater_project (host_id, project_id, created_at) VALUES (?, ?, ?)")
    .bind(owner, id, now)
    .run();
  const existing = await findProject(db, owner);
  if (!existing) throw new Error("project mapping missing after insert");
  return existing;
}

/** What running a capability needs, for either identity. */
export interface CapabilityEnv extends BillingEnv {
  DB: D1Database;
  API: Fetcher;
  PUBLIC_URL: string;
  HOOKS_URL?: string;
  NEW_TRIALS_PER_DAY?: string;
  /** Tool calls, per owner: one Agent Auth host or one MCP connection. */
  TOOL_RATE: RateLimit;
}

/** Runs a capability for an owner: create_project allocates its project, anything else needs one. */
export async function runCapability(
  env: CapabilityEnv,
  key: CryptoKey,
  owner: string,
  capability: string,
  args: Record<string, unknown>,
): Promise<unknown> {
  // Budgets bound what costs attempts; this bounds the rest (reads, and calls the API refuses).
  if (!(await env.TOOL_RATE.limit({ key: owner })).success) {
    throw new ApiError(429, {
      error: { code: "rate_limited", message: `more than ${TOOL_CALLS_PER_MINUTE} calls in a minute; wait a minute and try again` },
    });
  }
  const trialsPerDay = env.NEW_TRIALS_PER_DAY ? Number(env.NEW_TRIALS_PER_DAY) : undefined;
  const projectId = capability === "create_project" ? await ensureProject(env.DB, owner, trialsPerDay) : await findProject(env.DB, owner);
  if (!projectId) {
    throw new ApiError(404, {
      error: { code: "project_not_found", message: "call create_project before using other capabilities" },
    });
  }
  if (capability === "get_payment_link") {
    const extra = Object.keys(args)[0];
    if (extra !== undefined) throw new ArgumentError(`unknown argument ${JSON.stringify(extra)}`, extra);
    return paymentLink(env, owner, projectId);
  }
  try {
    const result = await execute(new ApiClient(env.API, key, projectId), capability, args);
    if (capability === "create_hook") {
      // The API knows only the path; the URL is shown this once and never stored.
      const { url_path, ...hook } = result as { url_path?: unknown } & Record<string, unknown>;
      if (typeof url_path !== "string") throw new ApiError(503, { error: { code: "api_unavailable", message: "the API did not return the hook's URL" } });
      return { ...hook, url: `${env.HOOKS_URL ?? env.PUBLIC_URL}${url_path}` };
    }
    return result;
  } catch (e) {
    if (e instanceof ApiError && OUT_OF_ATTEMPTS.has(e.body.error.code) && billingEnabled(env)) {
      const error = { ...e.body.error, message: `${e.body.error.message}; get_payment_link returns a link your user can use to buy more` };
      throw new ApiError(e.status, { error });
    }
    throw e;
  }
}
