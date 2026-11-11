// Who /a2a says is calling. The whole gateway runs here in Node, with Better Auth and the Agent
// Auth plugin on an in-memory SQLite behind D1's interface, so a test can see the caller the SDK's
// dispatcher is handed; the e2e suite covers the same route under workerd, from outside.

import { readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { DatabaseSync, type SQLInputValue } from "node:sqlite";
import { fileURLToPath } from "node:url";
import { JsonRpcTransportHandler, type ServerCallContext } from "@a2a-js/sdk/server";
import { AgentAuthClient, MemoryStorage } from "@auth/agent";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Caller } from "../src/a2a";
import worker, { type Env } from "../src/index";

const ORIGIN = "https://gateway.test";
const MIGRATIONS = join(dirname(fileURLToPath(import.meta.url)), "../migrations");

/** As much of D1 as Better Auth and the gateway use, over node:sqlite. */
function d1(db: DatabaseSync) {
  const statement = (sql: string, params: SQLInputValue[] = []) => ({
    bind: (...values: SQLInputValue[]) => statement(sql, values),
    first: async () => db.prepare(sql).get(...params) ?? null,
    all: async () => {
      const results = db.prepare(sql).all(...params);
      return { success: true, results, meta: db.prepare("SELECT changes() AS changes, last_insert_rowid() AS last_row_id").get() };
    },
  });
  return { prepare: (sql: string) => statement(sql), batch: async (all: ReturnType<typeof statement>[]) => Promise.all(all.map((s) => s.all())), exec: async () => undefined };
}

/** AuthStore's methods over a Map; nothing here waits long enough for an entry to expire. */
function authStore(kv: Map<string, string>) {
  const store = {
    get: async (key: string) => kv.get(key) ?? null,
    set: async (key: string, value: string) => void kv.set(key, value),
    delete: async (key: string) => void kv.delete(key),
    getAndDelete: async (key: string) => {
      const value = kv.get(key) ?? null;
      kv.delete(key);
      return value;
    },
    increment: async (key: string) => {
      const next = Number(kv.get(key) ?? 0) + 1;
      kv.set(key, String(next));
      return next;
    },
    claim: async (key: string) => !kv.has(key) && Boolean(kv.set(key, "1")),
  };
  return { idFromName: () => "auth-store", get: () => store };
}

const ctx = { waitUntil() {}, passThroughOnException() {} } as unknown as ExecutionContext;
let sqlite: DatabaseSync;
/** What the AuthStore holds: replay claims, the plugin's own jti cache and rate-limit counters. */
let stored: Map<string, string>;
let env: Env;
const gateway = (input: RequestInfo | URL, init?: RequestInit) => worker.fetch(new Request(input, init), env, ctx);

beforeEach(() => {
  sqlite = new DatabaseSync(":memory:");
  for (const file of readdirSync(MIGRATIONS).sort()) sqlite.exec(readFileSync(join(MIGRATIONS, file), "utf8"));
  stored = new Map();
  env = {
    PUBLIC_URL: ORIGIN,
    A2A_ENABLED: "true",
    ASSERTION_PRIVATE_KEY: "07".repeat(32),
    BETTER_AUTH_SECRET: "better-auth-secret-value-for-tests",
    DB: d1(sqlite),
    AUTH_STORE: authStore(stored),
    REGISTER_RATE: { limit: async () => ({ success: true }) },
  } as unknown as Env;
});
afterEach(() => {
  vi.restoreAllMocks();
  sqlite.close();
});

/** Registers an agent with the real SDK and says who /a2a takes it for when it calls. */
async function register(
  capabilities: string[],
  // No directory: the client discovers the gateway directly, and every request stays in this process.
  client = new AgentAuthClient({ storage: new MemoryStorage(), fetch: gateway, directoryUrl: "", allowDirectDiscovery: true, hostName: "caller test" }),
) {
  const { agentId, hostId } = await client.connectAgent({ provider: ORIGIN, mode: "autonomous", capabilities, name: "caller" });
  const sign = async () => (await client.signJwt({ agentId, audience: `${ORIGIN}/a2a`, capabilities: ["a2a"] })).token;
  const callA2a = async (token?: string) => {
    const handle = vi.spyOn(JsonRpcTransportHandler.prototype, "handle");
    token ??= await sign();
    const res = await gateway(`${ORIGIN}/a2a`, {
      method: "POST",
      headers: { "A2A-Version": "1.0", Authorization: `Bearer ${token}` },
      // An operation that uses no capability, so the answer is the same whatever the agent holds.
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "SubscribeToTask", params: { id: "task_1" } }),
    });
    const context = handle.mock.calls.at(-1)?.[1] as ServerCallContext | undefined;
    handle.mockRestore();
    return { status: res.status, caller: context?.user };
  };
  return { client, agentId, hostId, sign, callA2a };
}
const grant = (agentId: string, capability: string, set: string, value: string) =>
  sqlite.prepare(`UPDATE "agentCapabilityGrant" SET ${set} = ? WHERE "agentId" = ? AND capability = ?`).run(value, agentId, capability);

describe("the caller /a2a hands to the SDK's dispatcher", () => {
  it("is the agent's host, with the capabilities the agent holds", async () => {
    const agent = await register(["a2a", "schedule_http", "get_task"]);
    const { status, caller } = await agent.callA2a();
    expect(status).toBe(200);
    expect(caller).toBeInstanceOf(Caller);
    expect(caller).toMatchObject({ isAuthenticated: true, userName: agent.hostId, hostId: agent.hostId });
    expect([...(caller as Caller).grants].sort()).toEqual(["a2a", "get_task", "schedule_http"]);
  });

  it("is the same host for every agent of that host, each with its own capabilities", async () => {
    const first = await register(["a2a", "schedule_http"]);
    const second = await register(["a2a", "get_task"], first.client);
    const stranger = await register(["a2a", "cancel_task"]);
    const callers = [(await first.callA2a()).caller, (await second.callA2a()).caller, (await stranger.callA2a()).caller] as Caller[];
    expect(callers.map((c) => c.hostId)).toEqual([first.hostId, first.hostId, stranger.hostId]);
    expect(stranger.hostId).not.toBe(first.hostId);
    expect(callers.map((c) => [...c.grants].sort())).toEqual([["a2a", "schedule_http"], ["a2a", "get_task"], ["a2a", "cancel_task"]]);
  });

  it("leaves out a capability whose grant is revoked, expired or constrained", async () => {
    const agent = await register(["a2a", "schedule_http", "get_task", "list_tasks", "cancel_task"]);
    grant(agent.agentId, "schedule_http", "status", "revoked");
    grant(agent.agentId, "get_task", '"expiresAt"', new Date(Date.now() - 1000).toISOString());
    // Only the plugin's execute route checks a grant's constraints, so /a2a does not count one that has any.
    grant(agent.agentId, "list_tasks", "constraints", JSON.stringify({ status: { eq: "failed" } }));
    const { caller } = await agent.callA2a();
    expect([...(caller as Caller).grants].sort()).toEqual(["a2a", "cancel_task"]);
  });

  // Two copies of a request can both pass the plugin's replay check, which looks and then stores;
  // the gateway's claim is one atomic step in the AuthStore, shared with the Agent Auth routes.
  it("is not made for a token the gateway has seen, even when the plugin's own replay check has not", async () => {
    const agent = await register(["a2a"]);
    const claim = (token: string) => `replay::${agent.agentId}:${(JSON.parse(Buffer.from(token.split(".")[1]!, "base64url").toString()) as { jti: string }).jti}`;
    const seen = await agent.sign();
    stored.set(claim(seen), "1");
    expect(await agent.callA2a(seen)).toEqual({ status: 401, caller: undefined });

    const fresh = await agent.sign();
    expect(stored.has(claim(fresh))).toBe(false);
    expect((await agent.callA2a(fresh)).status).toBe(200);
    expect(stored.has(claim(fresh))).toBe(true);
  });

  it("is not made for an agent that is refused: the dispatcher is never asked", async () => {
    const agent = await register(["a2a", "schedule_http"]);
    grant(agent.agentId, "a2a", "status", "revoked");
    expect(await agent.callA2a()).toEqual({ status: 403, caller: undefined });
  });
});
