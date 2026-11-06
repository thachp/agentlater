// Staging test fixture. Records every request sent to /hook/<run>/<name> so the staging
// journey can check what AgentLater delivered, and scripts a few replies to exercise retries:
//   .../ok     200
//   .../flaky  503 the first time, then 200
//   .../gone   404
// Reads (GET /runs/<run>) need the RECEIVER_TOKEN bearer token. Each run is its own Durable
// Object and deletes itself an hour after its first request.

import { DurableObject } from "cloudflare:workers";

export interface Env {
  INBOX: DurableObjectNamespace<Inbox>;
  RECEIVER_TOKEN: string;
}

export interface Delivery {
  name: string;
  method: string;
  headers: Record<string, string>;
  body: string;
  received_at: number;
}

const RUN_RE = /^[a-z0-9-]{8,64}$/;
const NAME_RE = /^[a-z0-9-]{1,32}$/;
const MAX_BODY = 128 * 1024;
const MAX_ENTRIES = 200;
const TTL_MS = 60 * 60 * 1000;

export class Inbox extends DurableObject<Env> {
  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS delivery (seq INTEGER PRIMARY KEY, name TEXT, method TEXT, headers TEXT, body TEXT, received_at INTEGER)",
    );
  }

  /** Records a delivery and returns the status to reply with. */
  async record(d: Delivery): Promise<number> {
    const count = this.ctx.storage.sql.exec<{ n: number }>("SELECT COUNT(*) AS n FROM delivery").one().n;
    if (count >= MAX_ENTRIES) return 507;
    const earlier = this.ctx.storage.sql
      .exec<{ n: number }>("SELECT COUNT(*) AS n FROM delivery WHERE name = ?", d.name)
      .one().n;
    this.ctx.storage.sql.exec(
      "INSERT INTO delivery (name, method, headers, body, received_at) VALUES (?, ?, ?, ?, ?)",
      d.name,
      d.method,
      JSON.stringify(d.headers),
      d.body,
      d.received_at,
    );
    if ((await this.ctx.storage.getAlarm()) === null) await this.ctx.storage.setAlarm(Date.now() + TTL_MS);
    if (d.name === "gone") return 404;
    if (d.name === "flaky" && earlier === 0) return 503;
    return 200;
  }

  async list(): Promise<Delivery[]> {
    return this.ctx.storage.sql
      .exec<{ name: string; method: string; headers: string; body: string; received_at: number }>(
        "SELECT name, method, headers, body, received_at FROM delivery ORDER BY seq",
      )
      .toArray()
      .map((r) => ({ ...r, headers: JSON.parse(r.headers) as Record<string, string> }));
  }

  override async alarm(): Promise<void> {
    await this.ctx.storage.deleteAll();
  }
}

function sameToken(a: string, b: string): boolean {
  const x = new TextEncoder().encode(a);
  const y = new TextEncoder().encode(b);
  if (x.length !== y.length) return false;
  let diff = 0;
  for (let i = 0; i < x.length; i++) diff |= x[i]! ^ y[i]!;
  return diff === 0;
}

const json = (body: unknown, status = 200) => Response.json(body, { status, headers: { "Cache-Control": "no-store" } });

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const parts = new URL(request.url).pathname.split("/").filter(Boolean);

    if (parts.length === 1 && parts[0] === "health") return json({ service: "runlater-receiver", status: "ok" });

    if (parts[0] === "hook" && parts.length === 3 && RUN_RE.test(parts[1]!) && NAME_RE.test(parts[2]!)) {
      const body = await request.text();
      if (body.length > MAX_BODY) return json({ error: "too_large" }, 413);
      const headers: Record<string, string> = {};
      request.headers.forEach((v, k) => (headers[k] = v));
      const status = await env.INBOX.get(env.INBOX.idFromName(parts[1]!)).record({
        name: parts[2]!,
        method: request.method,
        headers,
        body,
        received_at: Date.now(),
      });
      return json({ recorded: status !== 507 }, status);
    }

    if (request.method === "GET" && parts[0] === "runs" && parts.length === 2 && RUN_RE.test(parts[1]!)) {
      const token = request.headers.get("Authorization")?.replace(/^Bearer\s+/i, "") ?? "";
      if (!env.RECEIVER_TOKEN || !sameToken(token, env.RECEIVER_TOKEN)) return json({ error: "unauthorized" }, 401);
      return json({ deliveries: await env.INBOX.get(env.INBOX.idFromName(parts[1]!)).list() });
    }

    return json({ error: "not_found" }, 404);
  },
} satisfies ExportedHandler<Env>;
