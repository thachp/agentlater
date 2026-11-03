// Maps an owner to its RunLater project: an Agent Auth host id, or `mcp:<id>` for a hosted MCP
// connection. One project per owner, never reassigned.

import { ApiClient, ApiError } from "./api";
import { billingEnabled, paymentLink, type BillingEnv } from "./billing";
import { ArgumentError, execute } from "./capabilities";

/** Errors that buying attempts fixes; the agent is told how. */
const OUT_OF_ATTEMPTS = new Set(["budget_exhausted", "trial_expired"]);

export async function findProject(db: D1Database, owner: string): Promise<string | null> {
  const row = await db
    .prepare("SELECT project_id FROM runlater_project WHERE host_id = ?")
    .bind(owner)
    .first<{ project_id: string }>();
  return row?.project_id ?? null;
}

/** Returns the owner's project id, allocating one on first call. Safe to repeat. */
export async function ensureProject(db: D1Database, owner: string): Promise<string> {
  const id = `prj_${[...crypto.getRandomValues(new Uint8Array(12))].map((b) => b.toString(16).padStart(2, "0")).join("")}`;
  await db
    .prepare("INSERT OR IGNORE INTO runlater_project (host_id, project_id, created_at) VALUES (?, ?, ?)")
    .bind(owner, id, Date.now())
    .run();
  const existing = await findProject(db, owner);
  if (!existing) throw new Error("project mapping missing after insert");
  return existing;
}

/** Runs a capability for an owner: create_project allocates its project, anything else needs one. */
export async function runCapability(
  env: BillingEnv & { DB: D1Database; API: Fetcher; PUBLIC_URL: string; HOOKS_URL?: string },
  key: CryptoKey,
  owner: string,
  capability: string,
  args: Record<string, unknown>,
): Promise<unknown> {
  const projectId = capability === "create_project" ? await ensureProject(env.DB, owner) : await findProject(env.DB, owner);
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
      const { url_path, ...hook } = result as { url_path: string } & Record<string, unknown>;
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
