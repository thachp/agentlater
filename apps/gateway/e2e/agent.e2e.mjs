// End-to-end: an agent, using the real Agent Auth SDK and the real MCP bridge, talks to the
// built gateway, which calls the built Rust API, which delivers to a captured destination.
// Both Workers run in one local workerd (Miniflare) with D1, Durable Objects and Queues.
//
// Build first: `npm run build` in apps/api and `npm run check` in apps/gateway.

import { after, before, describe, test } from "node:test";
import assert from "node:assert/strict";
import { createHmac, createPrivateKey, createPublicKey, randomBytes } from "node:crypto";
import { mkdtemp, readFile, readdir, rm } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { Miniflare, convertV4MiniflareOptions } from "miniflare";
import { AgentAuthClient, MemoryStorage } from "@auth/agent";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { createHash } from "node:crypto";
import { Challenge, Credential, Receipt } from "mppx";
import { runJourney } from "./journey.mjs";
import { REDIRECT, connectHosted } from "./oauth.mjs";

const gatewayDir = fileURLToPath(new URL("..", import.meta.url));
const apiDir = fileURLToPath(new URL("../../api/", import.meta.url));
const receiverDir = fileURLToPath(new URL("../../receiver/", import.meta.url));
const RECEIVER_HOST = "receiver.example.com";
const receiverToken = randomBytes(16).toString("hex");
const CAPABILITIES = [
  "create_project", "get_project", "get_payment_link", "schedule_http", "get_task", "list_tasks", "cancel_task", "redrive_task",
  "create_cron", "get_cron", "list_crons", "delete_cron", "watch_http",
];

// Gateway → API assertion keys, fresh per run.
const seed = randomBytes(32);
const privateKey = createPrivateKey({
  key: Buffer.concat([Buffer.from("302e020100300506032b657004220420", "hex"), seed]),
  format: "der",
  type: "pkcs8",
});
const publicHex = createPublicKey(privateKey).export({ format: "der", type: "spki" }).subarray(-32).toString("hex");

const freePort = () =>
  new Promise((resolve) => {
    const s = createServer().listen(0, "127.0.0.1", () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
  });

/** Fake Stripe: the Checkout Sessions the gateway created, as form fields. */
const STRIPE_API_BASE = "https://stripe.test";
const STRIPE_WEBHOOK_SECRET = "whsec_e2e";
const stripeSessions = [];
/** Fake Stripe: MPP PaymentIntents by idempotency key, as Stripe replays them. */
const paymentIntents = new Map();

/** Deliveries the API sent out, and scripted replies per path (default 200). */
const received = [];
const replies = new Map();

let mf;
let origin;
/** The staging receiver fixture, on its own port; the API reaches it as RECEIVER_HOST. */
let receiver;
let receiverOrigin;
before(async () => {
  const receiverPort = await freePort();
  receiverOrigin = `http://127.0.0.1:${receiverPort}`;
  receiver = new Miniflare(
    convertV4MiniflareOptions({
      host: "127.0.0.1",
      port: receiverPort,
      modulesRoot: join(receiverDir, "dist"),
      modules: [{ type: "ESModule", path: join(receiverDir, "dist/index.js") }],
      compatibilityDate: "2026-10-27",
      durableObjects: { INBOX: { className: "Inbox", useSQLite: true } },
      bindings: { RECEIVER_TOKEN: receiverToken },
    }),
  );
  await receiver.ready;

  const port = await freePort();
  origin = `http://127.0.0.1:${port}`;
  mf = new Miniflare(
    convertV4MiniflareOptions({
      host: "127.0.0.1",
      port,
      workers: [
        {
          name: "gateway",
          modules: true,
          modulesRoot: join(gatewayDir, "dist"),
          modules: [{ type: "ESModule", path: join(gatewayDir, "dist/index.js") }],
          compatibilityDate: "2026-10-27",
          compatibilityFlags: ["nodejs_compat"],
          bindings: {
            PUBLIC_URL: origin,
            ASSERTION_PRIVATE_KEY: seed.toString("hex"),
            BETTER_AUTH_SECRET: randomBytes(32).toString("hex"),
            STRIPE_SECRET_KEY: "sk_test_e2e",
            STRIPE_PUBLISHABLE_KEY: "pk_test_e2e",
            STRIPE_WEBHOOK_SECRET,
            STRIPE_API_BASE,
            STRIPE_PROFILE_ID: "profile_test_e2e",
          },
          serviceBindings: { API: "api" },
          d1Databases: { DB: "gateway-db" },
          durableObjects: { AUTH_STORE: { className: "AuthStore", useSQLite: true } },
          ratelimits: {
            REGISTER_RATE: { namespace_id: "1001", simple: { limit: 100, period: 60 } },
            OAUTH_RATE: { namespace_id: "1003", simple: { limit: 100, period: 60 } },
            MPP_RATE: { namespace_id: "1005", simple: { limit: 100, period: 60 } },
          },
          kvNamespaces: { OAUTH_KV: "oauth-kv" },
          outboundService: async (req) => {
            if (req.url === "https://api.stripe.com/v1/payment_intents" && req.method === "POST") {
              assert.equal(req.headers.get("authorization"), `Basic ${btoa("sk_test_e2e:")}`);
              const fields = Object.fromEntries(new URLSearchParams(await req.text()));
              const idem = req.headers.get("idempotency-key");
              const replayed = paymentIntents.has(idem);
              if (!replayed) paymentIntents.set(idem, { id: `pi_test_${paymentIntents.size + 1}`, status: "succeeded", fields });
              const pi = paymentIntents.get(idem);
              return Response.json({ id: pi.id, status: pi.status }, { headers: replayed ? { "Idempotent-Replayed": "true" } : {} });
            }
            if (req.url !== `${STRIPE_API_BASE}/v1/checkout/sessions` || req.method !== "POST") {
              throw new Error("the gateway must not reach the internet");
            }
            assert.equal(req.headers.get("authorization"), "Bearer sk_test_e2e");
            const fields = Object.fromEntries(new URLSearchParams(await req.text()));
            const id = `cs_test_${stripeSessions.length + 1}`;
            stripeSessions.push({ id, fields });
            return Response.json({ id, client_secret: `${id}_secret_e2e`, expires_at: Math.floor(Date.now() / 1000) + 86_400 });
          },
        },
        {
          name: "api",
          modulesRoot: apiDir,
          modules: [
            { type: "ESModule", path: join(apiDir, "build/index.js") },
            { type: "CompiledWasm", path: join(apiDir, "build/index_bg.wasm") },
          ],
          compatibilityDate: "2026-10-27",
          durableObjects: { LEDGER: { className: "TaskLedger", useSQLite: true } },
          queueProducers: { READY: { queueName: "runlater-ready" } },
          queueConsumers: { "runlater-ready": { maxBatchTimeout: 1 } },
          bindings: { GATEWAY_PUBLIC_KEY: publicHex },
          outboundService: async (req) => {
            if (new URL(req.url).hostname === RECEIVER_HOST) {
              const body = ["GET", "HEAD"].includes(req.method) ? undefined : await req.arrayBuffer();
              const res = await receiver.dispatchFetch(req.url, { method: req.method, headers: req.headers, body });
              return new Response(await res.arrayBuffer(), { status: res.status, headers: res.headers });
            }
            received.push({ url: req.url, headers: Object.fromEntries(req.headers), body: await req.text() });
            const script = replies.get(new URL(req.url).pathname) ?? [];
            const status = script.length ? script.shift() : 200;
            return new Response("", { status });
          },
        },
      ],
    }),
  );
  await mf.ready;
  const db = await mf.getD1Database("DB", "gateway");
  for (const file of (await readdir(join(gatewayDir, "migrations"))).sort()) {
    const sql = await readFile(join(gatewayDir, "migrations", file), "utf8");
    await db.batch(splitSql(sql).map((s) => db.prepare(s)));
  }
});
after(async () => {
  await mf?.dispose();
  await receiver?.dispose();
});

/** A migration file's statements, without comments. */
function splitSql(sql) {
  return sql.split(";").map((s) => s.replace(/^\s*--.*$/gm, "").trim()).filter(Boolean);
}

/** Every request the SDK made, so tests can replay one. */
const sent = [];
/** When set, the next execute request is sent twice at once; statuses land here. */
let duplicateNext = null;
const transport = async (url, init) => {
  if (duplicateNext && String(url).endsWith("/capability/execute")) {
    const record = duplicateNext;
    duplicateNext = null;
    const [a, b] = await Promise.all([fetch(url, init), fetch(url, init)]);
    record.statuses = [a.status, b.status].sort();
    return a.ok ? a : b;
  }
  const response = await fetch(url, init);
  sent.push({ url: String(url), init, response: response.clone() });
  return response;
};

function sdk() {
  return new AgentAuthClient({
    storage: new MemoryStorage(),
    fetch: transport,
    directoryUrl: null,
    allowDirectDiscovery: true,
    hostName: "RunLater e2e",
  });
}

async function until(check, timeoutMs = 20_000) {
  const end = Date.now() + timeoutMs;
  for (;;) {
    const value = await check();
    if (value) return value;
    if (Date.now() > end) throw new Error("timed out waiting");
    await new Promise((r) => setTimeout(r, 250));
  }
}

const data = (r) => r.data ?? r;

describe("agent journey through the gateway", () => {
  let client;
  let agentId;
  let project;

  test("health reaches the API through the binding", async () => {
    const res = await fetch(`${origin}/health`);
    assert.equal(res.status, 200);
    assert.equal((await res.json()).api, "ok");
  });

  test("discovery advertises only the served endpoints", async () => {
    const config = await (await fetch(`${origin}/.well-known/agent-configuration`)).json();
    assert.equal(config.provider_name, "RunLater");
    const paths = Object.values(config.endpoints).map((u) => new URL(u).pathname);
    assert.ok(paths.includes("/api/auth/capability/execute"));
    assert.ok(paths.includes("/api/auth/agent/request-capability"));
    assert.ok(paths.every((p) => !p.includes("/agent/list") && !p.includes("/host/create")));
  });

  test("an autonomous agent registers with no human account", async () => {
    client = sdk();
    const connection = await client.connectAgent({
      provider: origin,
      mode: "autonomous",
      capabilities: CAPABILITIES,
      name: "e2e agent",
    });
    agentId = connection.agentId;
    assert.ok(agentId);
  });

  test("capabilities need a project first", async () => {
    await assert.rejects(client.executeCapability({ agentId, capability: "get_project", arguments: {} }), {
      code: "project_not_found",
      status: 404,
    });
  });

  test("create_project is idempotent", async () => {
    project = data(await client.executeCapability({ agentId, capability: "create_project", arguments: {} }));
    assert.match(project.id, /^prj_[0-9a-f]{24}$/);
    assert.equal(project.attempts_remaining, 100);
    const again = data(await client.executeCapability({ agentId, capability: "create_project", arguments: {} }));
    assert.equal(again.id, project.id);
    assert.equal(again.signing_secret, project.signing_secret);
  });

  test("schedule_http delivers a signed request", async () => {
    const body = JSON.stringify({ order: 42 });
    const task = data(
      await client.executeCapability({
        agentId,
        capability: "schedule_http",
        arguments: { url: "https://dest.example.com/hook", body, headers: { "Content-Type": "application/json" } },
      }),
    );
    assert.equal(task.status, "scheduled");
    const done = await until(async () => {
      const t = data(await client.executeCapability({ agentId, capability: "get_task", arguments: { task_id: task.id } }));
      return t.status === "succeeded" && t;
    });
    assert.equal(done.attempts[0].http_status, 200);

    const delivery = received.find((r) => r.headers["runlater-task-id"] === task.id);
    const ts = delivery.headers["runlater-timestamp"];
    const mac = createHmac("sha256", project.signing_secret).update(`${ts}.${task.id}.1.${body}`).digest("hex");
    assert.equal(delivery.headers["runlater-signature"], `v1=${mac}`);
    assert.equal(delivery.body, body);
  });

  test("API validation errors reach the agent with the field", async () => {
    await assert.rejects(
      client.executeCapability({ agentId, capability: "schedule_http", arguments: { url: "http://insecure.example.com/" } }),
      { code: "invalid_request", status: 400, message: "URL must use https://" },
    );
    // The SDK keeps code and status; the response body also names the field.
    const last = sent.findLast((s) => s.url.endsWith("/capability/execute"));
    assert.equal((await last.response.json()).field, "url");
  });

  test("crons can be created, read, listed and deleted", async () => {
    const run = (capability, args) => client.executeCapability({ agentId, capability, arguments: args }).then((r) => r.data);
    const cron = await run("create_cron", {
      name: "daily-backup",
      schedule: "0 6 * * *",
      url: "https://dest.example.com/backup",
      alert_url: "https://dest.example.com/alerts",
    });
    assert.equal(cron.health, "pending");
    assert.ok(cron.next_run_ms > Date.now());
    assert.equal(new Date(cron.next_run_ms).getUTCHours(), 6);
    assert.equal((await run("get_cron", { name: "daily-backup" })).schedule, "0 6 * * *");
    assert.deepEqual((await run("list_crons", {})).crons.map((c) => c.name), ["daily-backup"]);
    await assert.rejects(
      client.executeCapability({ agentId, capability: "create_cron", arguments: { name: "x", schedule: "* * * * *", url: "https://dest.example.com/" } }),
      { code: "invalid_request", status: 400 },
    );
    await assert.rejects(
      client.executeCapability({ agentId, capability: "get_cron", arguments: { name: ".." } }),
      { code: "invalid_request", status: 400 },
    );
    assert.equal((await run("delete_cron", { name: "daily-backup" })).name, "daily-backup");
    await assert.rejects(
      client.executeCapability({ agentId, capability: "get_cron", arguments: { name: "daily-backup" } }),
      { code: "cron_not_found", status: 404 },
    );
  });

  test("watch_http polls until the condition matches", async () => {
    const run = (capability, args) => client.executeCapability({ agentId, capability, arguments: args }).then((r) => r.data);
    const watch = await run("watch_http", {
      url: "https://dest.example.com/job-status",
      every: "5m",
      until: { status: [200] },
      deadline: "1h",
    });
    assert.equal(watch.watch.every_ms, 300_000);
    const done = await until(async () => {
      const t = await run("get_task", { task_id: watch.id });
      return t.status === "succeeded" && t;
    });
    assert.equal(done.watch.reason, "matched");
    assert.equal(done.watch.matched, 200);
    await assert.rejects(
      client.executeCapability({ agentId, capability: "watch_http", arguments: { url: "https://dest.example.com/", every: "1m", until: { status: [200] }, deadline: "1h" } }),
      { code: "invalid_request", status: 400 },
    );
  });

  test("task ids cannot be used to escape the API path", async () => {
    await assert.rejects(
      client.executeCapability({ agentId, capability: "get_task", arguments: { task_id: "../projects/self" } }),
      { code: "invalid_request", status: 400 },
    );
  });

  test("a captured execute request cannot be replayed", async () => {
    await client.executeCapability({ agentId, capability: "get_project", arguments: {} });
    const last = sent.findLast((s) => s.url.endsWith("/capability/execute"));
    const replay = await fetch(last.url, last.init);
    assert.equal(replay.status, 401);
  });

  // Local workerd runs both copies in one isolate, so this checks the outcome but does not
  // reproduce the cross-isolate race; test/health.test.ts covers the gateway's atomic claim.
  test("two copies of one request sent at once execute only once", async () => {
    const record = {};
    duplicateNext = record;
    await client.executeCapability({
      agentId,
      capability: "schedule_http",
      arguments: { url: "https://dest.example.com/once", run_at_ms: Date.now() + 3_600_000 },
    });
    assert.deepEqual(record.statuses, [200, 401]);
    const tasks = data(await client.executeCapability({ agentId, capability: "list_tasks", arguments: {} }));
    assert.equal(tasks.tasks.filter((t) => t.url === "https://dest.example.com/once").length, 1);
  });

  test("agents of a revoked host are refused even if not yet revoked themselves", async () => {
    const other = sdk();
    const conn = await other.connectAgent({ provider: origin, mode: "autonomous", capabilities: CAPABILITIES, name: "doomed" });
    await other.executeCapability({ agentId: conn.agentId, capability: "create_project", arguments: {} });
    // Simulate a host revocation that stopped before reaching its agents.
    const db = await mf.getD1Database("DB", "gateway");
    await db.prepare('UPDATE "agentHost" SET status = ? WHERE id = ?').bind("revoked", conn.hostId).run();
    await assert.rejects(other.executeCapability({ agentId: conn.agentId, capability: "get_project", arguments: {} }), {
      code: "host_revoked",
      status: 403,
    });
  });

  test("a host registered before a capability existed can still be granted it", async () => {
    // Agent Auth saves a host's capabilities when it activates and never widens them, so
    // hosts from before the cron tools were added hold only the original seven.
    const original = CAPABILITIES.slice(0, 7);
    const old = sdk();
    const conn = await old.connectAgent({ provider: origin, mode: "autonomous", capabilities: original, name: "old host" });
    const db = await mf.getD1Database("DB", "gateway");
    await db.prepare('UPDATE "agentHost" SET "defaultCapabilities" = ? WHERE id = ?').bind(JSON.stringify(original), conn.hostId).run();
    await old.executeCapability({ agentId: conn.agentId, capability: "create_project", arguments: {} });
    const listCrons = () => old.executeCapability({ agentId: conn.agentId, capability: "list_crons", arguments: {} });
    const request = () => old.requestCapability({ agentId: conn.agentId, capabilities: ["list_crons"] });

    await request();
    await assert.rejects(listCrons(), { code: "capability_not_granted" });

    for (const file of (await readdir(join(gatewayDir, "migrations"))).filter((f) => /^000[3-9]_/.test(f)).sort()) {
      await db.batch(splitSql(await readFile(join(gatewayDir, "migrations", file), "utf8")).map((q) => db.prepare(q)));
    }
    await request();
    assert.deepEqual(data(await listCrons()).crons, []);
  });

  test("another agent on the same host shares the project", async () => {
    const second = await client.connectAgent({ provider: origin, mode: "autonomous", capabilities: CAPABILITIES, name: "e2e agent 2" });
    const p = data(await client.executeCapability({ agentId: second.agentId, capability: "get_project", arguments: {} }));
    assert.equal(p.id, project.id);
  });

  test("another host cannot see this project's tasks", async () => {
    const tasks = data(await client.executeCapability({ agentId, capability: "list_tasks", arguments: {} }));
    const taskId = tasks.tasks[0].id;
    const other = sdk();
    const { agentId: otherAgent } = await other.connectAgent({ provider: origin, mode: "autonomous", capabilities: CAPABILITIES, name: "stranger" });
    await other.executeCapability({ agentId: otherAgent, capability: "create_project", arguments: {} });
    await assert.rejects(
      other.executeCapability({ agentId: otherAgent, capability: "get_task", arguments: { task_id: taskId } }),
      { code: "task_not_found", status: 404 },
    );
  });
});

describe("MCP bridge (@auth/agent-cli mcp)", () => {
  let mcp;
  let storageDir;

  before(async () => {
    storageDir = await mkdtemp(join(tmpdir(), "runlater-mcp-"));
    mcp = new Client({ name: "runlater-e2e", version: "1.0.0" });
    await mcp.connect(
      new StdioClientTransport({
        command: process.execPath,
        args: [
          fileURLToPath(import.meta.resolve("@auth/agent-cli")).replace(/index\.js$/, "index.js"),
          "mcp",
          "--url",
          origin,
          "--storage-dir",
          storageDir,
          "--no-browser",
        ],
        env: { ...process.env, AGENT_AUTH_ENCRYPTION_KEY: randomBytes(32).toString("hex") },
        stderr: "pipe",
      }),
    );
  });
  after(async () => {
    await mcp?.close();
    await rm(storageDir, { recursive: true, force: true });
  });

  const call = async (name, args) => {
    const res = await mcp.callTool({ name, arguments: args });
    const text = res.content.map((c) => c.text).join("");
    assert.ok(!res.isError, `${name} failed: ${text}`);
    return JSON.parse(text);
  };

  test("exposes the Agent Auth tools", async () => {
    const { tools } = await mcp.listTools();
    const names = tools.map((t) => t.name);
    for (const n of ["connect_agent", "list_capabilities", "execute_capability"]) assert.ok(names.includes(n), n);
  });

  test("an MCP client can connect, create a project and schedule a task", async () => {
    const connected = await call("connect_agent", {
      provider: origin,
      capabilities: CAPABILITIES,
      mode: "autonomous",
      name: "mcp agent",
    });
    const agentId = connected.agentId ?? connected.agent_id;
    assert.ok(agentId, JSON.stringify(connected));

    const caps = await call("list_capabilities", { provider: origin });
    const listed = JSON.stringify(caps);
    for (const c of CAPABILITIES) assert.ok(listed.includes(c), c);

    const exec = (capability, args) => call("execute_capability", { agent_id: agentId, capability, arguments: args });
    const created = data(await exec("create_project", {}));
    assert.match(created.id, /^prj_/);
    const task = data(await exec("schedule_http", { url: "https://dest.example.com/from-mcp" }));
    const done = await until(async () => {
      const t = data(await exec("get_task", { task_id: task.id }));
      return t.status === "succeeded" && t;
    });
    assert.equal(done.attempt_count, 1);
  });
});

describe("hosted MCP at /mcp", () => {
  const b64url = (b) => Buffer.from(b).toString("base64url");
  const form = (o) => new URLSearchParams(o);

  /** Registers a client and walks the consent page; returns the token response. */
  async function connect(clientName = "Test Client") {
    const r = await connectHosted(origin, clientName);
    assert.equal(r.pageHeaders.get("x-frame-options"), "DENY");
    assert.ok(r.html.includes(`Connect ${clientName} to RunLater?`), r.html);
    assert.ok(r.html.includes("127.0.0.1"), "the page names where it returns to");
    assert.equal(`${r.redirect.origin}${r.redirect.pathname}`, REDIRECT);
    return r;
  }

  async function mcpClient(accessToken) {
    const client = new Client({ name: "e2e", version: "1.0.0" });
    await client.connect(
      new StreamableHTTPClientTransport(new URL(`${origin}/mcp`), {
        requestInit: { headers: { Authorization: `Bearer ${accessToken}` } },
      }),
    );
    return client;
  }
  const value = (r) => JSON.parse(r.content[0].text);

  test("an unauthenticated request points the client at the authorization server", async () => {
    const res = await fetch(`${origin}/mcp`, { method: "POST", headers: { "Content-Type": "application/json" }, body: "{}" });
    assert.equal(res.status, 401);
    assert.match(res.headers.get("www-authenticate") ?? "", /resource_metadata=/);
    const meta = await (await fetch(`${origin}/.well-known/oauth-protected-resource/mcp`)).json();
    assert.equal(meta.resource, `${origin}/mcp`);
    const as = await (await fetch(`${origin}/.well-known/oauth-authorization-server`)).json();
    assert.equal(as.authorization_endpoint, `${origin}/authorize`);
    assert.equal(as.registration_endpoint, `${origin}/oauth/register`);
  });

  test("a client connects through the consent page and uses the capabilities as tools", async () => {
    const { access_token, refresh_token, client_id } = await connect();
    const client = await mcpClient(access_token);
    const { tools } = await client.listTools();
    assert.deepEqual(tools.map((t) => t.name).sort(), [...CAPABILITIES].sort());
    assert.equal(tools.find((t) => t.name === "schedule_http").inputSchema.required[0], "url");

    const missing = await client.callTool({ name: "get_project", arguments: {} });
    assert.equal(missing.isError, true);
    assert.equal(value(missing).error.code, "project_not_found");

    const project = value(await client.callTool({ name: "create_project", arguments: {} }));
    assert.match(project.id, /^prj_/);
    const task = value(await client.callTool({ name: "schedule_http", arguments: { url: "https://dest.example.com/from-hosted-mcp" } }));
    const done = await until(async () => {
      const t = value(await client.callTool({ name: "get_task", arguments: { task_id: task.id } }));
      return t.status === "succeeded" && t;
    });
    assert.equal(done.attempt_count, 1);

    const bad = await client.callTool({ name: "schedule_http", arguments: { url: "http://dest.example.com/" } });
    assert.equal(bad.isError, true);
    assert.equal(value(bad).error.field, "url");
    await client.close();

    // A refreshed token is the same connection: same project.
    const refreshed = await (
      await fetch(`${origin}/oauth/token`, { method: "POST", body: form({ grant_type: "refresh_token", refresh_token, client_id }) })
    ).json();
    const again = await mcpClient(refreshed.access_token);
    assert.equal(value(await again.callTool({ name: "get_project", arguments: {} })).id, project.id);
    await again.close();

    // A second connection is a separate owner with its own project.
    const other = await mcpClient((await connect("Other Client")).access_token);
    const otherProject = value(await other.callTool({ name: "create_project", arguments: {} }));
    assert.notEqual(otherProject.id, project.id);
    const peek = await other.callTool({ name: "get_task", arguments: { task_id: task.id } });
    assert.equal(peek.isError, true);
    assert.equal(value(peek).error.code, "task_not_found");
    await other.close();
  });

  test("a person buys attempts through the payment link; Stripe's webhook adds them once", async () => {
    const client = await mcpClient((await connect("Buyer")).access_token);
    const project = value(await client.callTool({ name: "create_project", arguments: {} }));

    const link = value(await client.callTool({ name: "get_payment_link", arguments: {} }));
    assert.equal(link.attempts, 50_000);
    assert.deepEqual(link.price, { amount_cents: 500, currency: "usd" });
    const url = new URL(link.payment_url);
    assert.equal(`${url.origin}${url.pathname}`, "https://runlater.io/pay");
    assert.equal(url.search, "", "nothing secret in the query string");
    const fragment = new URLSearchParams(url.hash.slice(1));
    assert.equal(fragment.get("key"), "pk_test_e2e");
    const session = stripeSessions.at(-1);
    assert.equal(fragment.get("secret"), `${session.id}_secret_e2e`);
    assert.equal(session.fields.ui_mode, "embedded_page");
    assert.equal(session.fields["allowed_payment_method_types[0]"], "card");
    assert.equal(session.fields["line_items[0][price_data][unit_amount]"], "500");
    assert.equal(session.fields["metadata[project_id]"], project.id);

    const metadata = {
      owner: session.fields["metadata[owner]"],
      project_id: session.fields["metadata[project_id]"],
      attempts: session.fields["metadata[attempts]"],
    };
    const webhook = (type, paymentStatus, { secret = STRIPE_WEBHOOK_SECRET, meta = metadata } = {}) => {
      const body = JSON.stringify({ id: "evt_e2e", type, data: { object: { id: session.id, payment_status: paymentStatus, metadata: meta } } });
      const t = Math.floor(Date.now() / 1000);
      const sig = createHmac("sha256", secret).update(`${t}.${body}`).digest("hex");
      return fetch(`${origin}/billing/webhook`, { method: "POST", headers: { "Stripe-Signature": `t=${t},v1=${sig}` }, body });
    };
    const remaining = async () => value(await client.callTool({ name: "get_project", arguments: {} }));

    assert.equal((await webhook("checkout.session.completed", "paid", { secret: "whsec_wrong" })).status, 400);
    const t = Math.floor(Date.now() / 1000);
    const junk = await fetch(`${origin}/billing/webhook`, {
      method: "POST",
      headers: { "Stripe-Signature": `t=${t},v1=${createHmac("sha256", STRIPE_WEBHOOK_SECRET).update(`${t}.not json`).digest("hex")}` },
      body: "not json",
    });
    assert.equal(junk.status, 400);
    assert.equal((await webhook("checkout.session.completed", "unpaid")).status, 200);
    assert.equal((await remaining()).paid, false, "an unpaid session adds nothing");
    const forged = await webhook("checkout.session.completed", "paid", { meta: { ...metadata, project_id: "prj_000000000000000000000000" } });
    assert.equal(forged.status, 500, "a session must name its owner's own project");

    assert.equal((await webhook("checkout.session.completed", "paid")).status, 200);
    const after = await remaining();
    assert.equal(after.paid, true);
    assert.equal(after.attempts_remaining, project.attempts_remaining + 50_000);
    assert.ok(after.expires_ms > Date.now() + 360 * 86_400_000);

    assert.equal((await webhook("checkout.session.async_payment_succeeded", "paid")).status, 200);
    assert.equal((await remaining()).attempts_remaining, after.attempts_remaining, "the same session never adds twice");

    const extra = await client.callTool({ name: "get_payment_link", arguments: { amount: 10 } });
    assert.equal(extra.isError, true);
    await client.close();
  });

  test("an agent pays for itself over MPP; the pack is added once", async () => {
    const client = await mcpClient((await connect("MPP Buyer")).access_token);
    const project = value(await client.callTool({ name: "create_project", arguments: {} }));
    const link = value(await client.callTool({ name: "get_payment_link", arguments: {} }));
    assert.ok(link.payment_url, "Checkout stays available alongside MPP");
    const mppUrl = link.mpp_url.replace(/^https?:\/\/[^/]+/, origin);
    assert.ok(link.mpp_url.startsWith(`${origin}/billing/mpp/${project.id}.`));

    // No credential: a 402 with a Stripe charge for exactly one pack.
    const unpaid = await fetch(mppUrl, { method: "POST", body: "{}" });
    assert.equal(unpaid.status, 402);
    const challenge = Challenge.fromResponse(unpaid);
    assert.equal(challenge.method, "stripe");
    assert.equal(challenge.request.amount, "500");
    assert.equal(challenge.request.currency, "usd");
    assert.equal(challenge.request.methodDetails.networkId, "profile_test_e2e");

    // Pay with a (fake) Shared Payment Token.
    const authorization = Credential.serialize(Credential.from({ challenge, payload: { spt: "spt_test_e2e" } }));
    const paid = await fetch(mppUrl, { method: "POST", headers: { Authorization: authorization }, body: "{}" });
    assert.equal(paid.status, 200, await paid.clone().text());
    const receipt = Receipt.fromResponse(paid);
    const body = await paid.json();
    assert.equal(body.attempts_added, 50_000);
    assert.equal(body.payment_id, receipt.reference);
    const pi = [...paymentIntents.values()].find((p) => p.id === receipt.reference);
    assert.equal(pi.fields.amount, "500");
    assert.equal(pi.fields["metadata[runlater_project_id]"], project.id);
    assert.equal(pi.fields["shared_payment_granted_token"], "spt_test_e2e");
    const after = value(await client.callTool({ name: "get_project", arguments: {} }));
    assert.equal(after.attempts_remaining, project.attempts_remaining + 50_000);

    // The same credential again is refused, and Stripe's webhook for the same PaymentIntent adds nothing.
    const again = await fetch(mppUrl, { method: "POST", headers: { Authorization: authorization }, body: "{}" });
    assert.notEqual(again.status, 200);
    const event = JSON.stringify({
      id: "evt_pi",
      type: "payment_intent.succeeded",
      data: { object: { id: receipt.reference, metadata: { runlater_project_id: project.id, runlater_attempts: "50000" } } },
    });
    const t = Math.floor(Date.now() / 1000);
    const sig = createHmac("sha256", STRIPE_WEBHOOK_SECRET).update(`${t}.${event}`).digest("hex");
    const hook = await fetch(`${origin}/billing/webhook`, { method: "POST", headers: { "Stripe-Signature": `t=${t},v1=${sig}` }, body: event });
    assert.equal(hook.status, 200);
    assert.equal(value(await client.callTool({ name: "get_project", arguments: {} })).attempts_remaining, after.attempts_remaining);

    // A credential paid for this project's challenge is refused at another project's link.
    const other = await mcpClient((await connect("MPP Other")).access_token);
    await other.callTool({ name: "create_project", arguments: {} });
    const otherUrl = value(await other.callTool({ name: "get_payment_link", arguments: {} })).mpp_url.replace(/^https?:\/\/[^/]+/, origin);
    const fresh = Challenge.fromResponse(await fetch(mppUrl, { method: "POST", body: "{}" }));
    const stolen = Credential.serialize(Credential.from({ challenge: fresh, payload: { spt: "spt_test_stolen" } }));
    const intents = paymentIntents.size;
    assert.equal((await fetch(otherUrl, { method: "POST", headers: { Authorization: stolen }, body: "{}" })).status, 402);
    assert.equal(paymentIntents.size, intents, "nothing is charged");
    await other.close();

    // A link for another project, or a tampered one, is not accepted.
    const [, exp, mac] = new URL(mppUrl).pathname.split("/").pop().split(".");
    const forged = await fetch(`${origin}/billing/mpp/prj_ffffffffffffffffffffffff.${exp}.${mac}`, { method: "POST" });
    assert.equal(forged.status, 404);
    assert.equal((await fetch(mppUrl)).status, 405);
    await client.close();
  });

  test("Stripe's webhook ignores MPP charges it cannot apply, without retries", async () => {
    const send = async (metadata) => {
      const event = JSON.stringify({ id: "evt_x", type: "payment_intent.succeeded", data: { object: { id: "pi_test_x", metadata } } });
      const t = Math.floor(Date.now() / 1000);
      const sig = createHmac("sha256", STRIPE_WEBHOOK_SECRET).update(`${t}.${event}`).digest("hex");
      return fetch(`${origin}/billing/webhook`, { method: "POST", headers: { "Stripe-Signature": `t=${t},v1=${sig}` }, body: event });
    };
    assert.equal((await send({ runlater_project_id: "prj_ffffffffffffffffffffffff", runlater_attempts: "50000" })).status, 200);
    assert.equal((await send({ runlater_project_id: "prj_ffffffffffffffffffffffff" })).status, 200);
    assert.equal((await send({ runlater_project_id: "nope", runlater_attempts: "50000" })).status, 200);
  });

  test("Stripe's webhook credits an MPP charge that was not credited when it was made", async () => {
    const client = await mcpClient((await connect("MPP Backstop")).access_token);
    const project = value(await client.callTool({ name: "create_project", arguments: {} }));
    const event = JSON.stringify({
      id: "evt_pi2",
      type: "payment_intent.succeeded",
      data: { object: { id: "pi_test_backstop", metadata: { runlater_project_id: project.id, runlater_attempts: "50000" } } },
    });
    const t = Math.floor(Date.now() / 1000);
    const sig = createHmac("sha256", STRIPE_WEBHOOK_SECRET).update(`${t}.${event}`).digest("hex");
    assert.equal((await fetch(`${origin}/billing/webhook`, { method: "POST", headers: { "Stripe-Signature": `t=${t},v1=${sig}` }, body: event })).status, 200);
    const after = value(await client.callTool({ name: "get_project", arguments: {} }));
    assert.equal(after.attempts_remaining, project.attempts_remaining + 50_000);
    assert.equal(after.paid, true);
    await client.close();
  });

  test("/mcp only takes POST: no standalone stream, no session to delete", async () => {
    const { access_token } = await connect("Methods");
    for (const method of ["GET", "DELETE"]) {
      const res = await fetch(`${origin}/mcp`, { method, headers: { Authorization: `Bearer ${access_token}`, Accept: "text/event-stream" } });
      assert.equal(res.status, 405, method);
      assert.equal(res.headers.get("allow"), "POST");
    }
  });

  test("the consent page escapes what the client says about itself", async () => {
    const name = `<img src=x onerror=alert(1)>"'`;
    const r = await connectHosted(origin, name);
    assert.ok(!r.html.includes("<img"), "client_name reached the page unescaped");
    assert.ok(r.html.includes("&lt;img src=x onerror=alert(1)&gt;&quot;&#39;"));
    assert.ok(!/form-action/.test(r.pageHeaders.get("content-security-policy")), "form-action would block the redirect back");
  });

  test("declining sends access_denied back to the client", async () => {
    const reg = await (
      await fetch(`${origin}/oauth/register`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ client_name: "Declined", redirect_uris: [REDIRECT], token_endpoint_auth_method: "none" }),
      })
    ).json();
    const query = form({
      response_type: "code",
      client_id: reg.client_id,
      redirect_uri: REDIRECT,
      code_challenge: b64url(createHash("sha256").update("v".repeat(43)).digest()),
      code_challenge_method: "S256",
      state: "st-2",
    });
    const page = await fetch(`${origin}/authorize?${query}`);
    const handle = (await page.text()).match(/name="handle" value="([^"]+)"/)[1];
    const cookie = page.headers.getSetCookie().map((c) => c.split(";")[0]).join("; ");
    const deny = await fetch(`${origin}/authorize`, {
      method: "POST",
      redirect: "manual",
      headers: { Cookie: cookie },
      body: form({ handle, action: "deny" }),
    });
    assert.equal(deny.status, 302);
    assert.equal(new URL(deny.headers.get("location")).searchParams.get("error"), "access_denied");
  });

  test("a consent form cannot be approved without the browser that opened it", async () => {
    const reg = await (
      await fetch(`${origin}/oauth/register`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ client_name: "Forged", redirect_uris: [REDIRECT], token_endpoint_auth_method: "none" }),
      })
    ).json();
    const query = form({
      response_type: "code",
      client_id: reg.client_id,
      redirect_uri: REDIRECT,
      code_challenge: b64url(createHash("sha256").update("w".repeat(43)).digest()),
      code_challenge_method: "S256",
      state: "st-3",
    });
    const handle = (await (await fetch(`${origin}/authorize?${query}`)).text()).match(/name="handle" value="([^"]+)"/)[1];
    const forged = await fetch(`${origin}/authorize`, { method: "POST", redirect: "manual", body: form({ handle, action: "approve" }) });
    assert.equal(forged.status, 400);
  });
});

describe("staging journey, run against the local stack", () => {
  test("passes end to end through the receiver fixture", async () => {
    const result = await runJourney({
      gatewayUrl: origin,
      receiverUrl: `https://${RECEIVER_HOST}`,
      receiverReadUrl: receiverOrigin,
      receiverToken,
      timeoutMs: 60_000,
    });
    // ok + flaky (2) + gone + behind + delayed + its callback + one poll = 8 attempts;
    // canceled tasks and the deleted cron use none.
    assert.equal(result.attemptsUsed, 8);
  });
});
