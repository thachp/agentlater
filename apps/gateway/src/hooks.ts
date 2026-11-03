// Inbound hooks: the public URL a sender (GitHub, Stripe, a CI system) POSTs to. Served on the
// hooks host, outside Agent Auth: the token in the path is the credential. The gateway only
// bounds the request and hands it to the project's ledger, which checks the signature,
// applies the limits and writes the inbox event. Senders get a status and `{}`, never anything
// they sent.

import { signAssertion } from "./api";
import { readLimited } from "./body";

export interface HooksEnv {
  DB: D1Database;
  API: Fetcher;
  /** A cheap front door per hook URL; the ledger enforces the real per-hook limit. */
  HOOK_RATE: RateLimit;
}

const PATH_RE = /^\/h\/(prj_[0-9a-f]{24})\/(hk_[0-9a-f]{64})$/;
/** Matches MAX_HOOK_REQUEST_BYTES in crates/core/src/limits.rs. */
const MAX_HOOK_BYTES = 256 * 1024;

const answer = (status: number, headers: Record<string, string> = {}) =>
  new Response("{}", { status, headers: { "Content-Type": "application/json", "Cache-Control": "no-store", ...headers } });

function base64(bytes: Uint8Array): string {
  let binary = "";
  for (let i = 0; i < bytes.length; i += 0x8000) binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(binary);
}

export function isHookPath(pathname: string): boolean {
  return pathname === "/h" || pathname.startsWith("/h/");
}

export async function handleHook(request: Request, env: HooksEnv, key: CryptoKey): Promise<Response> {
  const match = new URL(request.url).pathname.match(PATH_RE);
  if (!match) return answer(404);
  const [, projectId, token] = match as [string, string, string];
  // Any other method, HEAD and GET included, so a URL can't be probed without sending a body.
  if (request.method !== "POST") return answer(405, { Allow: "POST" });
  if (!(await env.HOOK_RATE.limit({ key: `${projectId}/${token}` })).success) return answer(429, { "Retry-After": "60" });
  if (Number(request.headers.get("Content-Length") ?? 0) > MAX_HOOK_BYTES) return answer(413);
  const body = await readLimited(request, MAX_HOOK_BYTES);
  if (body === null) return answer(413);
  // Only projects that exist get a ledger: a made-up id must not create one.
  const known = await env.DB.prepare("SELECT 1 AS ok FROM runlater_project WHERE project_id = ?").bind(projectId).first();
  if (!known) return answer(404);

  const res = await env.API.fetch("https://api.internal/v1/hooks/_receive", {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "RunLater-Assertion": await signAssertion(key, projectId, ["hooks.receive"]),
    },
    body: JSON.stringify({ token, headers: [...request.headers], body: base64(body) }),
  });
  if (res.status >= 500) return answer(503, { "Retry-After": "60" });
  const retry = res.headers.get("Retry-After");
  return answer(res.status, retry ? { "Retry-After": retry } : {});
}
