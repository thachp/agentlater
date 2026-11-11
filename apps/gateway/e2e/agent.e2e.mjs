// End-to-end: an agent, using the real Agent Auth SDK and the real MCP bridge, talks to the
// built gateway, which calls the built Rust API, which delivers to a captured destination.
// Both Workers run in one local workerd (Miniflare) with D1, Durable Objects and Queues.
//
// Build first: `npm run build` in apps/api and `npm run check` in apps/gateway.

import { after, before, describe, test } from "node:test";
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { createHmac, createPrivateKey, createPublicKey, randomBytes, randomUUID, sign } from "node:crypto";
import { mkdtemp, readFile, readdir, rm, stat, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { Miniflare, convertV4MiniflareOptions } from "miniflare";
import { CancelTaskRequest, ListTasksRequest, SendMessageRequest, SubscribeToTaskRequest, TaskState } from "@a2a-js/sdk";
import { AgentCardResolver, ClientFactory, JsonRpcTransportFactory } from "@a2a-js/sdk/client";
import { ContentTypeNotSupportedError, RequestMalformedError, TaskNotCancelableError, TaskNotFoundError, UnsupportedOperationError } from "@a2a-js/sdk/errors";
import { AgentAuthClient, MemoryStorage } from "@auth/agent";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { createHash } from "node:crypto";
import { Challenge, Credential, Receipt } from "mppx";
import { apiWorker } from "../../api/test/api-worker.mjs";
import { connect as connectExample, delivery as exampleDelivery, deliveryOf, ended, run as runExample } from "../examples/a2a-client.mjs";
import { runA2aReport } from "./a2a-staging.mjs";
import { runJourney } from "./journey.mjs";
import { REDIRECT, connectHosted } from "./oauth.mjs";

const gatewayDir = fileURLToPath(new URL("..", import.meta.url));
const receiverDir = fileURLToPath(new URL("../../receiver/", import.meta.url));
const RECEIVER_HOST = "receiver.example.com";
/** The gateway's second domain; requests reach it through Miniflare's dispatchFetch. */
const ALT_ORIGIN = "https://alt.gateway.test";
const receiverToken = randomBytes(16).toString("hex");
const CAPABILITIES = [
  "create_project", "get_project", "rotate_signing_secret", "get_payment_link", "schedule_http", "queue_http", "get_task", "list_tasks", "cancel_task", "redrive_task",
  "create_cron", "get_cron", "list_crons", "delete_cron", "watch_http", "read_inbox", "ack_inbox", "set_wake", "delete_wake",
  "set_secret", "list_secrets", "delete_secret",
  "create_hook", "list_hooks", "update_hook", "delete_hook",
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

/**
 * Deliveries the API sent out, and scripted replies per path (default 200). A reply may be a
 * function: the destination holds the request until it resolves to the status.
 */
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
            ALT_PUBLIC_URL: ALT_ORIGIN,
            A2A_ENABLED: "true",
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
            HOOK_RATE: { namespace_id: "1007", simple: { limit: 100, period: 60 } },
            HOOK_PROJECT_RATE: { namespace_id: "1009", simple: { limit: 100, period: 60 } },
            TOOL_RATE: { namespace_id: "1011", simple: { limit: 1000, period: 60 } },
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
          ...apiWorker(publicHex),
          outboundService: async (req) => {
            if (new URL(req.url).hostname === RECEIVER_HOST) {
              const body = ["GET", "HEAD"].includes(req.method) ? undefined : await req.arrayBuffer();
              const res = await receiver.dispatchFetch(req.url, { method: req.method, headers: req.headers, body });
              return new Response(await res.arrayBuffer(), { status: res.status, headers: res.headers });
            }
            received.push({ url: req.url, headers: Object.fromEntries(req.headers), body: await req.text() });
            const script = replies.get(new URL(req.url).pathname) ?? [];
            const reply = script.length ? script.shift() : 200;
            return new Response("", { status: typeof reply === "function" ? await reply() : reply });
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
    hostName: "AgentLater e2e",
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
    assert.equal(config.provider_name, "agentlater");
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
    assert.equal(project.attempts_remaining, 1_000);
    const again = data(await client.executeCapability({ agentId, capability: "create_project", arguments: {} }));
    assert.equal(again.id, project.id);
    assert.match(project.signing_secret, /^rls_[0-9a-f]{64}$/);
    assert.equal(again.signing_secret, project.signing_secret);
    const read = data(await client.executeCapability({ agentId, capability: "get_project", arguments: {} }));
    assert.equal(read.id, project.id);
    assert.equal("signing_secret" in read, false, "only create_project returns the signing secret");
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

  test("rotate_signing_secret returns the new secret, and get_project only when the old one stops", async () => {
    const run = async (capability, args = {}) => data(await client.executeCapability({ agentId, capability, arguments: args }));
    const rotated = await run("rotate_signing_secret", { overlap: "1h" });
    assert.match(rotated.signing_secret, /^rls_[0-9a-f]{64}$/);
    assert.notEqual(rotated.signing_secret, project.signing_secret);
    assert.ok(rotated.previous_valid_until_ms > Date.now());
    const read = await run("get_project");
    assert.equal("signing_secret" in read, false, "get_project still leaves the signing secret out");
    assert.equal(read.signing_secret_previous_until_ms, rotated.previous_valid_until_ms);
    await assert.rejects(run("rotate_signing_secret", { overlap: "never" }), { code: "invalid_request", status: 400 });
    // Later tests verify deliveries with the secret they hold.
    project = await run("create_project");
    assert.equal(project.signing_secret, rotated.signing_secret);
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
    const reporting = await run("create_cron", {
      name: "daily-backup",
      schedule: "0 6 * * *",
      url: "https://dest.example.com/backup",
      callback_url: "inbox",
      capture: true,
      report: "failures",
    });
    assert.deepEqual([reporting.callback_url, reporting.capture, reporting.report], ["inbox", true, "failures"]);
    await assert.rejects(
      client.executeCapability({ agentId, capability: "create_cron", arguments: { name: "y", schedule: "0 6 * * *", url: "https://dest.example.com/", report: "failures" } }),
      { code: "invalid_request", status: 400 },
    );
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

  test("a hook turns a signed webhook into an inbox event, once", async () => {
    const exec = (capability, args = {}) => client.executeCapability({ agentId, capability, arguments: args }).then(data);
    const secret = "gh-webhook-secret-0123456789";
    const hook = await exec("create_hook", { name: "gh-ci", verify: { scheme: "github", secret } });
    assert.match(hook.url, /^http:\/\/127\.0\.0\.1:\d+\/h\/prj_[0-9a-f]{24}\/hk_[0-9a-f]{64}$/);
    assert.equal(hook.url_path, undefined);
    assert.equal(JSON.stringify(hook).includes(secret), false);

    const body = JSON.stringify({ action: "completed", workflow_run: { conclusion: "success" } });
    const signed = (b, delivery = "d-1") => ({
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "User-Agent": "GitHub-Hookshot/e2e",
        "X-GitHub-Event": "workflow_run",
        "X-GitHub-Delivery": delivery,
        "X-Hub-Signature-256": `sha256=${createHmac("sha256", secret).update(b).digest("hex")}`,
        Authorization: "Bearer must-not-be-kept",
      },
      body: b,
    });
    const first = await fetch(hook.url, signed(body));
    assert.equal(first.status, 202);
    assert.deepEqual(await first.json(), {});
    assert.equal((await fetch(hook.url, signed(body))).status, 200, "GitHub's retry is not written again");
    const forged = await fetch(hook.url, { ...signed(body), body: body.replace("success", "failure") });
    assert.equal(forged.status, 401);
    assert.equal((await fetch(hook.url)).status, 405);
    assert.equal((await fetch(hook.url.replace(/hk_[0-9a-f]{4}/, "hk_0000"), signed(body))).status, 404);
    const unknownProject = hook.url.replace(/prj_[0-9a-f]{24}/, `prj_${"0".repeat(24)}`);
    assert.equal((await fetch(unknownProject, signed(body))).status, 404);

    const page = await exec("read_inbox", {});
    const event = page.events.find((e) => e.event === "hook.received");
    assert.equal(event.hook, "gh-ci");
    assert.equal(event.verified, true);
    assert.equal(event.delivery_id, "d-1");
    assert.equal(event.request.body, body);
    assert.equal(event.request.headers["x-github-event"], "workflow_run");
    assert.equal(event.request.headers.authorization, undefined);
    assert.equal(page.events.filter((e) => e.event === "hook.received").length, 1);

    const [listed] = (await exec("list_hooks")).hooks;
    assert.equal(listed.received, 1);
    assert.equal(listed.refused.signature_invalid, 1);
    assert.equal(JSON.stringify(listed).includes("hk_"), false, "list_hooks never shows the URL");
    // A filter keeps only failed runs, and changing it keeps the URL.
    const updated = await exec("update_hook", {
      name: "gh-ci",
      accept: [{ headers: { "X-GitHub-Event": ["workflow_run"] }, path: "/workflow_run/conclusion", in: ["failure"] }],
    });
    assert.equal(updated.url, undefined);
    assert.equal(updated.accept[0].headers["x-github-event"][0], "workflow_run");
    const failed = JSON.stringify({ action: "completed", workflow_run: { conclusion: "failure" } });
    assert.equal((await fetch(hook.url, signed(body, "d-3"))).status, 202, "a success is answered like the rest");
    assert.equal((await fetch(hook.url, signed(failed, "d-4"))).status, 202);
    const kept = (await exec("read_inbox", { after: event.seq })).events.filter((e) => e.event === "hook.received");
    assert.deepEqual(kept.map((e) => e.delivery_id), ["d-4"], "only the failure is kept");
    const [filtered] = (await exec("list_hooks")).hooks;
    assert.equal(filtered.filtered, 1);
    assert.equal(filtered.received, 2);

    await exec("delete_hook", { name: "gh-ci" });
    assert.equal((await fetch(hook.url, signed(body, "d-2"))).status, 404);
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

describe("A2A at /a2a", () => {
  const rpc = (headers) =>
    fetch(`${origin}/a2a`, {
      method: "POST",
      headers: { "Content-Type": "application/json", ...headers },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "GetTask", params: { id: "task_00000000000000000000" } }),
    });

  test("the official A2A client reads the Agent Card and finds the JSON-RPC interface", async () => {
    const card = await AgentCardResolver.default.resolve(origin);
    assert.equal(card.name, "AgentLater");
    assert.deepEqual(
      card.supportedInterfaces.map((i) => [i.url, i.protocolBinding, i.protocolVersion]),
      [[`${origin}/a2a`, "JSONRPC", "1.0"]],
    );
    assert.equal(card.capabilities.extensions[0].params.discovery, `${origin}/.well-known/agent-configuration`);
    // The client picks that interface and sends A2A-Version: 1.0; without a token it is turned away.
    const client = await new ClientFactory().createFromAgentCard(card);
    await assert.rejects(client.getTask({ id: "task_00000000000000000000" }), /Status: 401/);
  });

  test("a request without A2A-Version: 1.0 gets the SDK's VersionNotSupportedError", async () => {
    const res = await rpc({});
    assert.equal(res.status, 200);
    const { error } = await res.json();
    assert.equal(error.code, -32009);
    assert.equal(error.data[0].reason, "VERSION_NOT_SUPPORTED");
  });

  const a2a = () => `${origin}/a2a`;
  const execute = () => `${origin}/api/auth/capability/execute`;
  /** A new client address for each request unless told otherwise, so none spends another's rate limit. Tens of thousands of them. */
  let hosts = 0;
  const address = () => `100.64.${++hosts >> 8}.${hosts & 255}`;
  const call = (token, { ip = address(), to = a2a(), via = fetch } = {}) =>
    via(to, {
      method: "POST",
      headers: { "Content-Type": "application/json", "A2A-Version": "1.0", "CF-Connecting-IP": ip, ...(token ? { Authorization: `Bearer ${token}` } : {}) },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "SubscribeToTask", params: { id: "task_00000000000000000000" } }),
    });
  /** The request reached the SDK's dispatcher, which answers it with an operation that uses no capability and is not supported. */
  const dispatched = async (res, why = "") => {
    const body = await res.json();
    assert.equal(res.status, 200, `${why} ${JSON.stringify(body)}`);
    assert.equal(body.error.code, -32004, why);
  };
  /** The request was turned away at HTTP, before JSON-RPC. */
  const refused = async (res, status, code, host = origin) => {
    const body = await res.json();
    assert.equal(res.status, status, JSON.stringify(body));
    assert.equal(body.error, code);
    assert.equal(body.jsonrpc, undefined);
    assert.equal(res.headers.get("WWW-Authenticate"), status === 401 ? `AgentAuth discovery="${host}/.well-known/agent-configuration"` : null);
  };

  /** A client at an address of its own: Better Auth allows ten registrations a minute from one. */
  const sdkAt = (ip) =>
    new AgentAuthClient({
      storage: new MemoryStorage(),
      fetch: (url, init) => fetch(url, { ...init, headers: { ...init.headers, "X-Forwarded-For": ip } }),
      directoryUrl: null,
      allowDirectDiscovery: true,
      hostName: "AgentLater e2e",
    });

  /** Registers an agent with the real SDK. `token` is what a caller signs for each request; `forge` signs what the SDK would not. */
  async function register(capabilities = ["a2a", ...CAPABILITIES], client = sdkAt(address())) {
    const conn = await client.connectAgent({ provider: origin, mode: "autonomous", capabilities, name: "a2a caller" });
    const token = async (audience = a2a(), caps = ["a2a"]) => (await client.signJwt({ agentId: conn.agentId, audience, capabilities: caps ?? undefined })).token;
    const signed = (keypair, header, claims) => {
      const now = Math.floor(Date.now() / 1000);
      const part = (o) => Buffer.from(JSON.stringify(o)).toString("base64url");
      const signing = `${part({ alg: "EdDSA", ...header })}.${part({ aud: a2a(), capabilities: ["a2a"], iat: now, exp: now + 60, jti: randomUUID(), ...claims })}`;
      return `${signing}.${sign(null, Buffer.from(signing), createPrivateKey({ key: keypair.privateKey, format: "jwk" })).toString("base64url")}`;
    };
    const forge = async (claims = {}, header = {}) => signed((await client.getConnection(conn.agentId)).agentKeypair, { typ: "agent+jwt", ...header }, { sub: conn.agentId, ...claims });
    /** The host's own JWT, which the SDK signs only for the issuer. */
    const hostToken = async (claims = {}) => signed((await client.storage.getHostIdentity()).keypair, { typ: "host+jwt" }, { iss: conn.hostId, sub: conn.hostId, ...claims });
    return { client, agentId: conn.agentId, hostId: conn.hostId, token, forge, hostToken };
  }
  /**
   * A request to an Agent Auth route, from an address of its own: Better Auth allows 60 a minute to most of them.
   * `authorization` is the whole header, for one a client library would not write.
   */
  const agentAuth = (path, token, body, authorization = `Bearer ${token}`) =>
    fetch(`${origin}/api/auth${path}`, {
      method: body ? "POST" : "GET",
      headers: { "Content-Type": "application/json", Authorization: authorization, "X-Forwarded-For": address() },
      body: body && JSON.stringify(body),
    });
  const gatewayDb = () => mf.getD1Database("DB", "gateway");

  let caller;
  before(async () => {
    caller = await register();
  });

  /** The official A2A client for an agent, with a fetch that signs a new token for every request, as the card says to. */
  async function a2aClient(agent) {
    const card = await AgentCardResolver.default.resolve(origin);
    const { audience, capability } = card.capabilities.extensions[0].params;
    const fetchImpl = async (url, init) =>
      fetch(url, { ...init, headers: { ...Object.fromEntries(new Headers(init.headers)), "CF-Connecting-IP": address(), Authorization: `Bearer ${await agent.token(audience, [capability])}` } });
    return new ClientFactory({ transports: [new JsonRpcTransportFactory({ fetchImpl })] }).createFromAgentCard(card);
  }

  test("an agent JWT signed for /a2a with the a2a capability reaches the SDK's dispatcher", async () => {
    const client = await a2aClient(caller);
    // Streaming is the operation that stays unsupported.
    await assert.rejects(client.resubscribeTask(SubscribeToTaskRequest.fromJSON({ id: "task_00000000000000000000" })).next(), UnsupportedOperationError);
    await dispatched(await call(await caller.token()));
    await dispatched(await call(await caller.forge()), "a hand-signed token with the same claims is the same token");
  });

  test("a new host is given the a2a capability", async () => {
    const host = await (await gatewayDb()).prepare('SELECT "defaultCapabilities" FROM "agentHost" WHERE id = ?').bind(caller.hostId).first();
    assert.deepEqual(JSON.parse(host.defaultCapabilities), [...CAPABILITIES, "a2a"]);
  });

  test("a request without a token is refused", async () => {
    await refused(await call(null), 401, "authentication_required");
  });

  test("a malformed token is refused", async () => {
    const good = await caller.token();
    for (const token of ["not-a-jwt", "a.b.c", "e30.e30.e30", good.split(".").slice(0, 2).join("."), `${good}.extra`]) {
      await refused(await call(token), 401, "invalid_jwt");
    }
  });

  test("a token with a wrong signature is refused", async () => {
    const [header, payload, signature] = (await caller.token()).split(".");
    const other = await register();
    const claims = JSON.parse(Buffer.from(payload, "base64url"));
    const tampered = Buffer.from(JSON.stringify({ ...claims, jti: randomUUID() })).toString("base64url");
    await refused(await call(`${header}.${tampered}.${signature}`), 401, "invalid_jwt");
    await refused(await call(`${header}.${payload}.${(await other.token()).split(".")[2]}`), 401, "invalid_jwt");
    // Another agent's key, claiming to be this agent.
    await refused(await call(await other.forge({ sub: caller.agentId })), 401, "invalid_jwt");
    await refused(await call(await caller.forge({}, { alg: "none" })), 401, "invalid_jwt");
  });

  test("an expired token is refused, and so is one older than a minute or not an agent JWT", async () => {
    const now = Math.floor(Date.now() / 1000);
    await refused(await call(await caller.forge({ iat: now - 120, exp: now - 60 })), 401, "invalid_jwt");
    await refused(await call(await caller.forge({ iat: now - 120, exp: now + 60 })), 401, "invalid_jwt");
    await refused(await call(await caller.forge({ iat: undefined })), 401, "invalid_jwt");
    await refused(await call(await caller.forge({ iat: now + 600, exp: now + 660 })), 401, "invalid_jwt");
    await refused(await call(await caller.forge({}, { typ: "JWT" })), 401, "invalid_jwt");
    await refused(await call(await caller.forge({ jti: undefined })), 401, "invalid_jwt");
  });

  // The plugin checks htm and htu against its own session route, not /a2a, so a caller must leave them out.
  test("a token that binds itself to the request with htm or htu is refused", async () => {
    await refused(await call(await caller.forge({ htm: "POST", htu: a2a() })), 401, "request_binding_mismatch");
    await refused(await call(await caller.forge({ htu: a2a() })), 401, "request_binding_mismatch");
  });

  test("a token used once is refused the second time, on /a2a and on every other route", async () => {
    const token = await caller.token();
    await dispatched(await call(token));
    await refused(await call(token), 401, "jti_replay");
    // With an audience the other routes take, so that only its having been used is against it.
    const general = await caller.token(origin, null);
    assert.equal((await agentAuth(`/agent/status?agent_id=${caller.agentId}`, general)).status, 200);
    await refused(await call(general), 401, "jti_replay");
  });

  test("two copies of one request sent at once reach the dispatcher only once", async () => {
    const token = await caller.token();
    const [a, b] = await Promise.all([call(token), call(token)]);
    assert.deepEqual([a.status, b.status].sort(), [200, 401]);
  });

  test("a token signed for the capability-execution route is refused on /a2a", async () => {
    const config = await (await fetch(`${origin}/.well-known/agent-configuration`)).json();
    assert.equal(config.default_location, execute());
    // As the SDK signs it for executeCapability, then with a2a named, then with no capability named.
    for (const capabilities of [["schedule_http"], ["a2a"], null]) {
      await refused(await call(await caller.token(config.default_location, capabilities)), 401, "invalid_jwt");
    }
  });

  test("a token signed for the bare origin, the issuer or any other audience is refused on /a2a", async () => {
    for (const audience of [origin, `${origin}/api/auth`, `${a2a()}/`, `${a2a()}?x=1`, a2a().toUpperCase(), "https://gateway.agentlater.io/a2a"]) {
      for (const capabilities of [["a2a"], null]) await refused(await call(await caller.token(audience, capabilities)), 401, "invalid_jwt");
    }
    // A list that names /a2a among others is not a token for /a2a only.
    await refused(await call(await caller.forge({ aud: [a2a(), origin] })), 401, "invalid_jwt");
    await refused(await call(await caller.forge({ aud: [a2a()] })), 401, "invalid_jwt");
  });

  test("a token that does not name the a2a capability, and it alone, is refused", async () => {
    for (const capabilities of [null, ["schedule_http"], ["a2a", "schedule_http"], []]) {
      await refused(await call(await caller.forge({ capabilities: capabilities ?? undefined })), 401, "invalid_jwt");
    }
  });

  test("an agent that does not hold the a2a capability is refused with 403", async () => {
    const without = await register(CAPABILITIES);
    await assert.rejects(without.token(), { code: "capability_not_granted" }, "the SDK will not sign for a capability the agent lacks");
    await refused(await call(await without.forge()), 403, "capability_not_granted");

    const revoked = await register();
    await (await gatewayDb()).prepare('UPDATE "agentCapabilityGrant" SET status = ? WHERE "agentId" = ? AND capability = ?').bind("revoked", revoked.agentId, "a2a").run();
    await refused(await call(await revoked.token()), 403, "capability_not_granted");
    const expired = await register();
    await (await gatewayDb()).prepare('UPDATE "agentCapabilityGrant" SET "expiresAt" = ? WHERE "agentId" = ? AND capability = ?').bind(new Date(Date.now() - 1000).toISOString(), expired.agentId, "a2a").run();
    await refused(await call(await expired.token()), 403, "capability_not_granted");
  });

  test("a revoked agent is refused", async () => {
    const doomed = await register();
    const token = await doomed.token();
    await doomed.client.disconnectAgent(doomed.agentId);
    await refused(await call(token), 403, "agent_revoked");
  });

  test("an agent of a revoked host is refused, even if not yet revoked itself", async () => {
    const doomed = await register();
    await dispatched(await call(await doomed.token()));
    // A host revocation that stopped before reaching its agents.
    await (await gatewayDb()).prepare('UPDATE "agentHost" SET status = ? WHERE id = ?').bind("revoked", doomed.hostId).run();
    await refused(await call(await doomed.token()), 403, "host_revoked");
    await dispatched(await call(await caller.token()), "another host's agent is not affected");
  });

  test("a host's own JWT is refused", async () => {
    await refused(await call(await caller.hostToken()), 401, "invalid_jwt");
  });

  test("an OAuth token for hosted MCP is refused", async () => {
    const { access_token } = await connectHosted(origin, "A2A intruder");
    const mcp = await fetch(`${origin}/mcp`, {
      method: "POST",
      headers: { Authorization: `Bearer ${access_token}`, "Content-Type": "application/json", Accept: "application/json, text/event-stream" },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" }),
    });
    assert.equal(mcp.status, 200, "the token is good where it was issued for");
    await refused(await call(access_token), 401, "invalid_jwt");
  });

  test("a token signed for /a2a executes nothing on the capability-execution route, and is refused on every other Agent Auth route", async () => {
    await caller.client.executeCapability({ agentId: caller.agentId, capability: "create_project", arguments: {} });
    const tasks = async () => data(await caller.client.executeCapability({ agentId: caller.agentId, capability: "list_tasks", arguments: {} })).tasks.length;
    const before = await tasks();
    const attempts = [
      ["/capability/execute", { capability: "schedule_http", arguments: { url: "https://dest.example.com/must-not-run" } }],
      ["/capability/execute", { capability: "get_project", arguments: {} }],
      ["/capability/execute", { capability: "a2a", arguments: {} }],
      ["/agent/request-capability", { capabilities: ["schedule_http"] }],
      ["/agent/revoke", { agent_id: caller.agentId }],
      ["/agent/rotate-key", { public_key: { kty: "OKP", crv: "Ed25519", x: "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo" } }],
      [`/agent/status?agent_id=${caller.agentId}`, undefined],
      ["/capability/list", undefined],
    ];
    for (const [path, body] of attempts) {
      const res = await agentAuth(path, await caller.token(), body);
      assert.equal(res.status, 401, path);
      assert.deepEqual(await res.json(), { error: "invalid_jwt", message: "a token signed for /a2a is accepted only there" }, path);
    }
    assert.equal(await tasks(), before);
    assert.equal(received.some((r) => r.url === "https://dest.example.com/must-not-run"), false);
    await dispatched(await call(await caller.token()), "the agent was neither revoked nor given another key");
    // A list of audiences that names /a2a is refused here as well as there.
    const both = await agentAuth("/capability/execute", await caller.forge({ aud: [a2a(), execute()] }), { capability: "get_project", arguments: {} });
    assert.equal(both.status, 401);
    // The refusal is made only about a token the plugin has verified: the capability list is public, and a
    // token that fails verification there is ignored as before, whatever audience it claims.
    const unverified = `${(await caller.token()).split(".").slice(0, 2).join(".")}.${"A".repeat(86)}`;
    assert.equal((await agentAuth("/capability/list", unverified)).status, 200);
  });

  /** Ways to write whitespace or padding into a header that still carries a token's three segments. */
  const rewritten = (token) => {
    const [header, payload, signature] = token.split(".");
    const cut = (segment, gap) => `${segment.slice(0, 12)}${gap}${segment.slice(12)}`;
    return {
      "a space in the signature": `Bearer ${header}.${payload}.${cut(signature, " ")}`,
      "a tab in the signature": `Bearer ${header}.${payload}.${cut(signature, "\t")}`,
      "a space after the signature's first dot": `Bearer ${header}.${payload}. ${signature}`,
      "a space in the payload": `Bearer ${header}.${cut(payload, " ")}.${signature}`,
      "a space in the header": `Bearer ${cut(header, " ")}.${payload}.${signature}`,
      "padding after a space": `Bearer ${token} =`,
      "padding": `Bearer ${token}==`,
      "two spaces after Bearer": `Bearer  ${token}`,
      "a tab after Bearer": `Bearer\t${token}`,
    };
  };

  test("a token signed for /a2a executes nothing on the capability-execution route however whitespace or padding is written into its header", async () => {
    const victim = await register();
    await run(victim, "create_project");
    const count = async () => (await run(victim, "list_tasks")).tasks.length;
    assert.equal(await count(), 0);
    for (const what of Object.keys(rewritten("a.b.c"))) {
      const authorization = rewritten(await victim.token())[what];
      const res = await agentAuth("/capability/execute", null, { capability: "schedule_http", arguments: { url: "https://dest.example.com/must-not-run", delay: "1h" } }, authorization);
      assert.equal(res.status, 401, `${what}: ${await res.text()}`);
      assert.equal(await count(), 0, what);
    }
    // Nor does it revoke the agent or give it another key.
    for (const what of ["a space in the signature", "a tab in the signature"]) {
      for (const [path, body] of [["/agent/revoke", { agent_id: victim.agentId }], ["/agent/rotate-key", { public_key: { kty: "OKP", crv: "Ed25519", x: "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo" } }]]) {
        assert.equal((await agentAuth(path, null, body, rewritten(await victim.token())[what])).status, 401, `${path}, ${what}`);
      }
    }
    await dispatched(await call(await victim.token()), "the agent was neither revoked nor given another key");
  });

  test("a token whose header is not exactly Bearer and a compact JWS is refused on every Agent Auth route and on /a2a, and a request with no token is served as before", async () => {
    const agent = await register();
    await run(agent, "create_project");
    const routes = [
      ["/agent/register", {}],
      [`/agent/status?agent_id=${agent.agentId}`, undefined],
      ["/agent/revoke", { agent_id: agent.agentId }],
      ["/agent/rotate-key", { public_key: {} }],
      ["/agent/reactivate", { agent_id: agent.agentId }],
      ["/agent/request-capability", { capabilities: ["schedule_http"] }],
      ["/capability/list", undefined],
      ["/capability/describe?name=schedule_http", undefined],
      ["/capability/execute", { capability: "get_project", arguments: {} }],
      ["/host/revoke", {}],
      ["/host/rotate-key", { public_key: {} }],
    ];
    const malformed = async () => ({
      ...rewritten(await agent.token(execute(), ["get_project"])),
      "another scheme": "Basic dXNlcjpwYXNz",
      "no token": "Bearer",
      "an opaque token": "Bearer 3f9c1d:7a2b:5e8d4c",
      "two segments": "Bearer e30.e30",
      "four segments": "Bearer e30.e30.e30.e30",
      "an empty segment": "Bearer e30..e30",
      "a character outside base64url": "Bearer e30.e3+0.e30",
      "a token after a comma": `Bearer ${await agent.token(execute(), ["get_project"])}, Bearer e30.e30.e30`,
    });
    for (const [path, body] of routes) {
      for (const [what, authorization] of Object.entries(await malformed())) {
        const res = await agentAuth(path, null, body, authorization);
        assert.equal(res.status, 401, `${path}, ${what}`);
        assert.deepEqual(await res.json(), { error: "invalid_jwt", message: "the Authorization header must be Bearer, one space and a compact JWT" }, `${path}, ${what}`);
      }
    }
    for (const [what, authorization] of Object.entries(rewritten(await agent.token()))) {
      const res = await fetch(a2a(), { method: "POST", headers: { "A2A-Version": "1.0", "CF-Connecting-IP": address(), Authorization: authorization }, body: "{}" });
      assert.equal(res.status, 401, `/a2a, ${what}`);
      // Turned away as no bearer token at all, or by the same rule as the other routes: never by the plugin.
      const { error, message } = await res.json();
      if (error !== "authentication_required") assert.equal(message, "the Authorization header must be Bearer, one space and a compact JWT", `/a2a, ${what}`);
      assert.equal(error, /^Bearer\s+\S+$/.test(authorization) ? "invalid_jwt" : "authentication_required", `/a2a, ${what}`);
    }
    // Without the header the public routes answer, and with a well-formed one a caller is served, as before.
    assert.equal((await fetch(`${origin}/api/auth/capability/list`)).status, 200);
    assert.equal((await fetch(`${origin}/api/auth/capability/describe?name=schedule_http`)).status, 200);
    assert.equal((await run(agent, "get_project")).attempts_remaining, 1_000);
    assert.equal((await agentAuth("/capability/execute", null, { capability: "get_project", arguments: {} }, `bearer ${await agent.token(execute(), ["get_project"])}`)).status, 200, "the scheme's case does not matter");
  });

  test("a token whose jti is not a string is refused on every route, and one with a very long jti is taken once", async () => {
    const agent = await register();
    await run(agent, "create_project");
    const elsewhere = { aud: execute(), capabilities: ["get_project"] };
    // The plugin takes any jti that is not empty; the gateway's claim needs a string to claim.
    for (const jti of [7, true, ["a"], { a: 1 }]) {
      const res = await agentAuth("/capability/execute", await agent.forge({ ...elsewhere, jti }), { capability: "get_project", arguments: {} });
      assert.equal(res.status, 401, JSON.stringify(jti));
      assert.deepEqual(await res.json(), { error: "invalid_jwt", message: "this token has no jti" });
      await refused(await call(await agent.forge({ jti })), 401, "invalid_jwt");
    }
    const long = "j".repeat(300);
    const once = await agent.forge({ ...elsewhere, jti: long });
    assert.equal((await agentAuth("/capability/execute", once, { capability: "get_project", arguments: {} })).status, 200);
    const again = await agentAuth("/capability/execute", once, { capability: "get_project", arguments: {} });
    assert.equal(again.status, 401);
    assert.deepEqual(await again.json(), { error: "invalid_jwt", message: "this token has already been used" });
    const onA2a = await agent.forge({ jti: `${long}2` });
    const [a, b] = await Promise.all([call(onA2a), call(onA2a)]);
    assert.deepEqual([a.status, b.status].sort(), [200, 401]);
  });

  test("a token signed for /a2a is refused on the routes a host uses too, whether an agent or the host signed it", async () => {
    const agent = await register();
    const ours = { error: "invalid_jwt", message: "a token signed for /a2a is accepted only there" };
    const key = { kty: "OKP", crv: "Ed25519", x: "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo" };
    const attempts = [
      ["/capability/describe?name=schedule_http", undefined],
      ["/agent/reactivate", { agent_id: agent.agentId }],
      ["/host/revoke", {}],
      ["/host/rotate-key", { public_key: key }],
      ["/agent/revoke", { agent_id: agent.agentId }],
      [`/agent/status?agent_id=${agent.agentId}`, undefined],
    ];
    for (const [path, body] of attempts) {
      for (const [who, token] of [["the agent", await agent.token()], ["the host", await agent.hostToken()]]) {
        const res = await agentAuth(path, token, body);
        assert.equal(res.status, 401, `${path}, ${who}`);
        assert.deepEqual(await res.json(), ours, `${path}, ${who}`);
      }
    }
    // Registration verifies the host's JWT itself and takes no audience but the issuer's.
    assert.equal((await agentAuth("/agent/register", await agent.hostToken(), { name: "x", mode: "autonomous" })).status, 401);
    const host = await (await gatewayDb()).prepare('SELECT status, "publicKey" FROM "agentHost" WHERE id = ?').bind(agent.hostId).first();
    assert.equal(host.status, "active");
    assert.notEqual(JSON.parse(host.publicKey).x, key.x);
    await dispatched(await call(await agent.token()), "the agent and its host are as they were");

    // The same tokens signed for the issuer are the plugin's to answer: the hook turns away only /a2a's.
    const issuer = `${origin}/api/auth`;
    const served = [
      ["/capability/describe?name=schedule_http", undefined, await agent.token(issuer, null), 200],
      [`/agent/status?agent_id=${agent.agentId}`, undefined, await agent.hostToken({ aud: issuer, capabilities: undefined }), 200],
      ["/agent/reactivate", { agent_id: agent.agentId }, await agent.hostToken({ aud: issuer, capabilities: undefined }), undefined],
    ];
    for (const [path, body, token, status] of served) {
      const res = await agentAuth(path, token, body);
      const answer = await res.json();
      if (status) assert.equal(res.status, status, `${path}: ${JSON.stringify(answer)}`);
      assert.notEqual(answer.message, ours.message, path);
      assert.notEqual(answer.message, "the Authorization header must be Bearer, one space and a compact JWT", path);
    }
    const revoked = await agentAuth("/host/revoke", await agent.hostToken({ aud: issuer, capabilities: undefined }), {});
    assert.equal(revoked.status, 200, await revoked.clone().text());
    await refused(await call(await agent.token()), 403, "agent_revoked");
  });

  test("the a2a capability cannot be executed: it does nothing on the capability-execution route", async () => {
    const res = await agentAuth("/capability/execute", await caller.token(execute(), ["a2a"]), { capability: "a2a", arguments: {} });
    assert.equal(res.status, 400);
    assert.deepEqual(await res.json(), { error: "invalid_request", message: `a2a is not executed; it lets an agent call ${a2a()}` });
  });

  test("a token for one gateway domain's /a2a is refused on the other's", async () => {
    const onAlt = (url, init) => mf.dispatchFetch(url, init);
    const altA2a = `${ALT_ORIGIN}/a2a`;
    const card = await (await onAlt(`${ALT_ORIGIN}/.well-known/agent-card.json`)).json();
    assert.equal(card.capabilities.extensions[0].params.audience, altA2a);

    await refused(await call(await caller.token(), { to: altA2a, via: onAlt }), 401, "invalid_jwt", ALT_ORIGIN);
    await refused(await call(await caller.token(altA2a)), 401, "invalid_jwt");
    await dispatched(await call(await caller.token(altA2a), { to: altA2a, via: onAlt }));
  });

  test("an agent registered before a2a existed is refused until it is granted the capability", async () => {
    const old = await register(CAPABILITIES);
    const db = await gatewayDb();
    // Agent Auth saves a host's capabilities when it activates and never widens them.
    await db.prepare('UPDATE "agentHost" SET "defaultCapabilities" = ? WHERE id = ?').bind(JSON.stringify(CAPABILITIES), old.hostId).run();
    const request = () => old.client.requestCapability({ agentId: old.agentId, capabilities: ["a2a"] });
    await request();
    await assert.rejects(old.token(), { code: "capability_not_granted" });
    await refused(await call(await old.forge()), 403, "capability_not_granted");

    const migration = await readFile(join(gatewayDir, "migrations", "0016_a2a_capability.sql"), "utf8");
    await db.batch(splitSql(migration).map((q) => db.prepare(q)));
    await request();
    await dispatched(await call(await old.token()));
  });

  // ---- Delivery tasks: SendMessage, GetTask, CancelTask and ListTasks ----

  /** One JSON-RPC call to /a2a for an agent: the HTTP status and the body. */
  async function rpcAs(agent, method, params) {
    const res = await fetch(a2a(), {
      method: "POST",
      headers: { "Content-Type": "application/json", "A2A-Version": "1.0", "CF-Connecting-IP": address(), Authorization: `Bearer ${await agent.token()}` },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
    });
    return { status: res.status, body: await res.json() };
  }
  const message = (data, over = {}) => ({ message: { messageId: randomUUID(), role: "ROLE_USER", parts: [{ data, mediaType: "application/json" }], ...over } });
  /** Sends a payload and returns the task, which must have been made. */
  async function sendAs(agent, data, over) {
    const { status, body } = await rpcAs(agent, "SendMessage", message(data, over));
    assert.equal(status, 200, JSON.stringify(body));
    assert.ok(body.result?.task, JSON.stringify(body));
    return body.result.task;
  }
  const getAs = async (agent, id) => (await rpcAs(agent, "GetTask", { id })).body;
  /** What the task's one artifact says of its delivery. */
  const delivery = (task) => task.artifacts[0].parts[0].data;
  const TERMINAL = ["TASK_STATE_COMPLETED", "TASK_STATE_FAILED", "TASK_STATE_CANCELED"];
  const settledAs = (agent, id) => until(async () => { const { result } = await getAs(agent, id); return TERMINAL.includes(result.status.state) && result; }, 40_000);
  /** A capability run over Agent Auth, the interface that existed before A2A. */
  const run = async (agent, capability, args = {}) => data(await agent.client.executeCapability({ agentId: agent.agentId, capability, arguments: args }));
  /** A registered agent whose host has a project. */
  async function withProject(capabilities) {
    const agent = await register(capabilities);
    agent.project = await run(agent, "create_project");
    return agent;
  }
  const deliveriesTo = (path) => received.filter((r) => new URL(r.url).pathname === path);

  test("the official A2A client schedules a delayed delivery and queues an immediate one, gets each task at once, and reads it until it is terminal", async () => {
    const agent = await withProject();
    const client = await a2aClient(agent);
    const request = (data, configuration) => SendMessageRequest.fromJSON({ ...message(data), configuration });

    // An hour away, and the client asks to wait for the end (its default): the task still comes back at once.
    const started = Date.now();
    const later = await client.sendMessage(request({ url: "https://dest.example.com/a2a-later", delay: "1h" }));
    assert.ok(Date.now() - started < 10_000);
    assert.match(later.id, /^task_[0-9a-f]{20}$/);
    assert.equal(later.contextId, later.id);
    assert.equal(later.status.state, TaskState.TASK_STATE_SUBMITTED);
    const laterDelivery = later.artifacts[0].parts[0].content.value;
    assert.equal(laterDelivery.status, "scheduled");
    assert.ok(Math.abs(laterDelivery.run_at_ms - (started + 3_600_000)) < 60_000);
    assert.equal((await client.getTask({ id: later.id })).status.state, TaskState.TASK_STATE_SUBMITTED);
    assert.equal(deliveriesTo("/a2a-later").length, 0);

    // Now, with and without returnImmediately: the same, a task that is not finished yet.
    for (const [path, configuration] of [["/a2a-now", { returnImmediately: true }], ["/a2a-now-blocking", { returnImmediately: false }]]) {
      const now = await client.sendMessage(request({ url: `https://dest.example.com${path}`, method: "PUT", headers: { "X-Order": "42" }, body: '{"order":42}', capture: true }, configuration));
      assert.ok([TaskState.TASK_STATE_SUBMITTED, TaskState.TASK_STATE_WORKING].includes(now.status.state));
      const done = await until(async () => { const task = await client.getTask({ id: now.id }); return task.status.state === TaskState.TASK_STATE_COMPLETED && task; });
      const result = done.artifacts[0].parts[0].content.value;
      assert.equal(result.task_id, now.id);
      assert.equal(result.status, "succeeded");
      assert.equal(result.attempt_count, 1);
      assert.equal(result.attempts[0].http_status, 200);
      assert.ok(result.created_ms <= result.finished_ms);
      assert.equal(result.response.status, 200);
      assert.equal(new Date(done.status.timestamp).getTime(), result.updated_ms);
      const [sent] = deliveriesTo(path);
      assert.equal(sent.headers["x-order"], "42");
      assert.equal(sent.body, '{"order":42}');
      assert.equal(sent.headers["runlater-task-id"], now.id);
    }
    // The same task over the interface that was there before, with nothing added to what it returns.
    const viaAgentAuth = await run(agent, "get_task", { task_id: later.id });
    assert.equal(viaAgentAuth.status, "scheduled");
    assert.equal("via" in viaAgentAuth, false);
  });

  test("the payload schema is served at the URL the card's skill names, and says what the examples the card gives are", async () => {
    const card = await (await fetch(`${origin}/.well-known/agent-card.json`)).json();
    const url = `${origin}/a2a/schemas/deliver-http/v1.json`;
    assert.ok(card.skills[0].description.includes(url));
    const schema = await (await fetch(url)).json();
    assert.equal(schema.$id, url);
    assert.deepEqual(Object.keys(schema.properties), ["url", "method", "headers", "body", "run_at_ms", "delay", "idempotency_key", "max_attempts", "capture"]);
    for (const example of card.skills[0].examples) assert.deepEqual(Object.keys(JSON.parse(example)).filter((k) => !(k in schema.properties)), []);
  });

  test("a text-only or ambiguous message schedules nothing and says what to send; an invalid field is named", async () => {
    const agent = await withProject();
    const client = await a2aClient(agent);
    const text = SendMessageRequest.fromJSON({ message: { messageId: randomUUID(), role: "ROLE_USER", parts: [{ text: "POST to https://dest.example.com/a2a-text in 15 minutes" }] } });
    await assert.rejects(client.sendMessage(text), (e) => e instanceof ContentTypeNotSupportedError && e.message.includes(`${origin}/a2a/schemas/deliver-http/v1.json`));

    const refusedField = async (data, field, over) => {
      const { status, body } = await rpcAs(agent, "SendMessage", message(data, over));
      assert.equal(status, 200);
      assert.equal(body.error?.code, -32602, JSON.stringify(body));
      assert.equal(body.error.data[0].metadata.field, field, JSON.stringify(body));
      return body.error;
    };
    const two = await rpcAs(agent, "SendMessage", message(null, { parts: [{ data: { url: "https://dest.example.com/a2a-text" } }, { data: { delay: "15m" } }] }));
    assert.equal(two.body.error.code, -32602);
    assert.equal(two.body.error.data[0].metadata.field, "message.parts");
    // The API's own validation, with its message, under the field's place in the request.
    const insecure = await refusedField({ url: "http://dest.example.com/a2a-text" }, "message.parts[0].data.url");
    assert.equal(insecure.message, "URL must use https://");
    await refusedField({ url: "https://dest.example.com/a2a-text", delay: "soon" }, "message.parts[0].data.delay");
    await refusedField({ url: "https://dest.example.com/a2a-text", max_attempts: 99 }, "message.parts[0].data.max_attempts");
    await refusedField({ url: "https://127.0.0.1/a2a-text" }, "message.parts[0].data.url");
    // A request the API cannot read at all has no field of its own; its message says what is missing.
    const noUrl = await rpcAs(agent, "SendMessage", message({ delay: "1m" }));
    assert.equal(noUrl.body.error.code, -32602);
    assert.match(noUrl.body.error.message, /url/);
    // Fields of schedule_http that this version of the payload does not have.
    await refusedField({ url: "https://dest.example.com/a2a-text", callback_url: "inbox" }, "message.parts[0].data.callback_url");
    await refusedField({ url: "https://dest.example.com/a2a-text", lane: "a" }, "message.parts[0].data.lane");
    await refusedField({ url: "https://dest.example.com/a2a-text" }, "message.contextId", { contextId: randomUUID() });

    assert.deepEqual((await run(agent, "list_tasks")).tasks, []);
    assert.equal(deliveriesTo("/a2a-text").length, 0);
  });

  test("a message for a task is refused: the caller's own takes no further message, another host's or interface's is not found", async () => {
    const agent = await withProject();
    const stranger = await withProject();
    const client = await a2aClient(agent);
    const mine = await sendAs(agent, { url: "https://dest.example.com/a2a-follow-up", delay: "1h" });
    const theirs = await sendAs(stranger, { url: "https://dest.example.com/a2a-follow-up", delay: "1h" });
    const other = await run(agent, "schedule_http", { url: "https://dest.example.com/a2a-follow-up", delay: "1h" });
    const followUp = (taskId) => SendMessageRequest.fromJSON(message({ url: "https://dest.example.com/a2a-follow-up" }, { taskId }));

    await assert.rejects(client.sendMessage(followUp(mine.id)), UnsupportedOperationError);
    for (const id of [theirs.id, other.id, "task_00000000000000000000", "not-a-task-id"]) await assert.rejects(client.sendMessage(followUp(id)), TaskNotFoundError, id);
    assert.equal((await run(agent, "list_tasks")).tasks.length, 2);
    assert.equal(deliveriesTo("/a2a-follow-up").length, 0);
  });

  test("the same submission sent again returns the same task with one delivery; the same key with a different payload is refused with -32602", async () => {
    const agent = await withProject();
    const payload = { url: "https://dest.example.com/a2a-once", body: "once" };
    // No key in the payload: the messageId is the key, here one a key could not be as it is.
    const again = message(payload, { messageId: "urn:uuid:" + randomUUID() });
    const first = await rpcAs(agent, "SendMessage", again);
    const second = await rpcAs(agent, "SendMessage", again);
    assert.equal(second.body.result.task.id, first.body.result.task.id);
    const changed = await rpcAs(agent, "SendMessage", { message: { ...again.message, parts: [{ data: { ...payload, body: "twice" }, mediaType: "application/json" }] } });
    assert.equal(changed.status, 200);
    assert.equal(changed.body.error.code, -32602);
    assert.match(changed.body.error.message, /already used for a different request/);
    assert.deepEqual(changed.body.error.data[0].metadata, { field: "message.messageId", code: "idempotency_conflict" });

    // A key in the payload wins over the messageId: two messages, one task.
    const keyed = { url: "https://dest.example.com/a2a-once-keyed", delay: "1h", idempotency_key: "order-42" };
    const a = await sendAs(agent, keyed);
    const b = await sendAs(agent, keyed);
    assert.equal(b.id, a.id);
    const conflict = await rpcAs(agent, "SendMessage", message({ ...keyed, url: "https://dest.example.com/a2a-once-other" }));
    assert.equal(conflict.body.error.code, -32602);
    assert.deepEqual(conflict.body.error.data[0].metadata, { field: "message.parts[0].data.idempotency_key", code: "idempotency_conflict" });
    // A key another interface already used is not A2A's task to return.
    await run(agent, "schedule_http", { url: "https://dest.example.com/a2a-once-keyed", delay: "1h", idempotency_key: "order-43" });
    const elsewhere = await rpcAs(agent, "SendMessage", message({ ...keyed, idempotency_key: "order-43" }));
    assert.equal(elsewhere.body.error.code, -32602);

    await settledAs(agent, first.body.result.task.id);
    assert.equal((await run(agent, "list_tasks")).tasks.length, 3);
    assert.equal(deliveriesTo("/a2a-once").length, 1);
    assert.equal(deliveriesTo("/a2a-once")[0].body, "once");
    assert.equal(deliveriesTo("/a2a-once-other").length, 0);
  });

  test("a delivery being retried is working, never completed; one the destination refuses is failed; every attempt is charged and no read is", async () => {
    const agent = await withProject();
    const remaining = async () => (await run(agent, "get_project")).attempts_remaining;
    const before = await remaining();
    replies.set("/a2a-flaky", [503]);
    replies.set("/a2a-gone", [404]);
    const flaky = await sendAs(agent, { url: "https://dest.example.com/a2a-flaky", max_attempts: 2 });
    const gone = await sendAs(agent, { url: "https://dest.example.com/a2a-gone" });

    const retrying = await until(async () => { const { result } = await getAs(agent, flaky.id); return delivery(result).status === "retrying" && result; });
    assert.equal(retrying.status.state, "TASK_STATE_WORKING");
    assert.equal(delivery(retrying).attempt_count, 1);
    assert.equal(delivery(retrying).attempts[0].outcome, "http_503");
    assert.ok(delivery(retrying).next_attempt_ms > Date.now() - 1000);

    const failed = await settledAs(agent, gone.id);
    assert.equal(failed.status.state, "TASK_STATE_FAILED");
    assert.deepEqual([delivery(failed).status, delivery(failed).attempt_count, delivery(failed).attempts[0].http_status], ["failed", 1, 404]);
    const completed = await settledAs(agent, flaky.id);
    assert.equal(completed.status.state, "TASK_STATE_COMPLETED");
    assert.deepEqual(delivery(completed).attempts.map((a) => a.http_status), [503, 200]);

    // Three attempts were made, and charged as they are for any task; the reads above cost nothing.
    assert.equal(await remaining(), before - 3);
    for (let i = 0; i < 10; i++) await getAs(agent, flaky.id);
    assert.equal(await remaining(), before - 3);
    assert.equal(deliveriesTo("/a2a-flaky").length, 2);
  });

  test("no signing secret, credential header or stored secret value appears in any A2A answer", async () => {
    const agent = await withProject();
    const stored = `stored-${randomBytes(12).toString("hex")}`;
    const literal = `literal-${randomBytes(12).toString("hex")}`;
    const inUrl = `url-${randomBytes(12).toString("hex")}`;
    await run(agent, "set_secret", { name: "A2A_TOKEN", value: stored, allowed_hosts: ["dest.example.com"] });
    const payload = { url: `https://dest.example.com/a2a-secret/${inUrl}?key=${inUrl}`, headers: { Authorization: "Bearer {{secret:A2A_TOKEN}}", "X-Api-Key": literal }, body: literal, capture: true };
    const answers = [await rpcAs(agent, "SendMessage", message(payload))];
    const done = await settledAs(agent, answers[0].body.result.task.id);
    answers.push(done, await rpcAs(agent, "SendMessage", message(payload, { messageId: "another" })), await rpcAs(agent, "SendMessage", message({ ...payload, delay: "soon" })));
    assert.equal(done.status.state, "TASK_STATE_COMPLETED");
    // The destination did get the credentials.
    const sent = received.find((r) => r.url === payload.url);
    assert.equal(sent.headers.authorization, `Bearer ${stored}`);
    assert.equal(sent.headers["x-api-key"], literal);

    const text = JSON.stringify(answers);
    for (const secret of [agent.project.signing_secret, stored, literal, inUrl, "A2A_TOKEN", "Authorization"]) assert.equal(text.includes(secret), false, secret);
  });

  test("an agent needs the grant for the capability each operation uses; without it nothing is scheduled or read", async () => {
    const owner = await withProject();
    const task = await sendAs(owner, { url: "https://dest.example.com/a2a-grants", delay: "1h" });
    const refusedFor = async (agent, capability, method, params) => {
      const { status, body } = await rpcAs(agent, method, params);
      assert.equal(status, 403, JSON.stringify(body));
      assert.deepEqual(body, { error: "capability_not_granted", message: `this agent does not hold the ${capability} capability; request it, or register an agent with it` });
    };
    // Agents of the same host, so of the same project, each holding less.
    const reader = await register(["a2a", "get_task"], owner.client);
    await refusedFor(reader, "schedule_http", "SendMessage", message({ url: "https://dest.example.com/a2a-grants", delay: "1h" }));
    await refusedFor(reader, "queue_http", "SendMessage", message({ url: "https://dest.example.com/a2a-grants" }));
    assert.equal((await getAs(reader, task.id)).result.id, task.id);

    const queuer = await register(["a2a", "queue_http"], owner.client);
    await refusedFor(queuer, "schedule_http", "SendMessage", message({ url: "https://dest.example.com/a2a-grants", delay: "1h" }));
    await refusedFor(queuer, "get_task", "GetTask", { id: task.id });
    await refusedFor(queuer, "get_task", "GetTask", { id: "task_00000000000000000000" });
    await refusedFor(queuer, "get_task", "SendMessage", message({ url: "https://dest.example.com/a2a-grants" }, { taskId: task.id }));

    // A grant that is revoked stops counting at once.
    const revoked = await register(["a2a", "schedule_http", "queue_http", "get_task"], owner.client);
    await (await gatewayDb()).prepare('UPDATE "agentCapabilityGrant" SET status = ? WHERE "agentId" = ? AND capability = ?').bind("revoked", revoked.agentId, "queue_http").run();
    await refusedFor(revoked, "queue_http", "SendMessage", message({ url: "https://dest.example.com/a2a-grants" }));

    assert.equal((await run(owner, "list_tasks")).tasks.length, 1);
    assert.equal(deliveriesTo("/a2a-grants").length, 0);
    const queued = await sendAs(queuer, { url: "https://dest.example.com/a2a-grants" });
    assert.match(queued.id, /^task_/);
  });

  test("a task made by another host or through another interface is answered with TaskNotFoundError, like one that never was", async () => {
    const agent = await withProject();
    const stranger = await withProject();
    const client = await a2aClient(agent);
    const mine = await sendAs(agent, { url: "https://dest.example.com/a2a-mine", delay: "1h" });
    const theirs = await sendAs(stranger, { url: "https://dest.example.com/a2a-theirs", delay: "1h" });
    // The same host, over Agent Auth: an ordinary task, and a watch.
    const scheduled = await run(agent, "schedule_http", { url: "https://dest.example.com/a2a-other", delay: "1h" });
    const queued = await run(agent, "queue_http", { url: "https://dest.example.com/a2a-other", queue: "a2a" });
    // The mark is not an argument of any capability.
    await assert.rejects(run(agent, "schedule_http", { url: "https://dest.example.com/a2a-other", delay: "1h", via: "a2a" }), { code: "invalid_request", status: 400 });
    await assert.rejects(run(agent, "queue_http", { url: "https://dest.example.com/a2a-other", via: "a2a" }), { code: "invalid_request", status: 400 });

    assert.equal((await client.getTask({ id: mine.id })).id, mine.id);
    const never = await getAs(agent, "task_00000000000000000000");
    assert.equal(never.error.code, -32001);
    assert.deepEqual(never.error.data, [{ "@type": "type.googleapis.com/google.rpc.ErrorInfo", reason: "TASK_NOT_FOUND", domain: "a2a-protocol.org" }]);
    for (const id of [theirs.id, scheduled.id, queued.id, "not-a-task-id", "", "task_0000000000000000000g"]) {
      assert.deepEqual(await getAs(agent, id), never, id);
      await assert.rejects(client.getTask({ id }), TaskNotFoundError, id);
    }
    // A caller with no project at all is told the same.
    assert.deepEqual(await getAs(await register(), mine.id), never);
    // Each task is still there for whoever may read it: its own host through A2A, and the host's every task over Agent Auth.
    assert.equal((await getAs(stranger, theirs.id)).result.id, theirs.id);
    assert.deepEqual((await run(agent, "list_tasks")).tasks.map((t) => t.id).sort(), [mine.id, scheduled.id, queued.id].sort());
  });

  /** A destination that holds its next request until `answer(status)`. */
  function held(path) {
    let answer;
    const status = new Promise((resolve) => (answer = resolve));
    replies.set(path, [() => status]);
    return answer;
  }
  const NEVER = "task_00000000000000000000";
  const ids = (tasks) => tasks.map((t) => t.id);

  test("CancelTask cancels a task that has not started, answers TaskNotCancelableError for a finished one, and returns one in flight as working", async () => {
    const agent = await withProject();
    const client = await a2aClient(agent);
    const cancel = (id) => client.cancelTask(CancelTaskRequest.fromJSON({ id }));

    // Not started: canceled now, and never sent.
    const later = await sendAs(agent, { url: "https://dest.example.com/a2a-cancel-later", delay: "1h" });
    const canceled = await cancel(later.id);
    assert.equal(canceled.id, later.id);
    assert.equal(canceled.status.state, TaskState.TASK_STATE_CANCELED);
    assert.equal(canceled.status.message, undefined);
    assert.equal(canceled.artifacts[0].parts[0].content.value.status, "canceled");
    assert.equal((await getAs(agent, later.id)).result.status.state, "TASK_STATE_CANCELED");
    // Asked again, it is still canceled: a cancel that is repeated is not an error.
    assert.equal((await cancel(later.id)).status.state, TaskState.TASK_STATE_CANCELED);
    assert.equal((await run(agent, "get_task", { task_id: later.id })).status, "canceled");
    assert.equal(deliveriesTo("/a2a-cancel-later").length, 0);

    // Finished, either way: not cancelable, and unchanged.
    replies.set("/a2a-cancel-gone", [404]);
    const done = await sendAs(agent, { url: "https://dest.example.com/a2a-cancel-done" });
    const gone = await sendAs(agent, { url: "https://dest.example.com/a2a-cancel-gone" });
    for (const [task, state, how] of [[done, "TASK_STATE_COMPLETED", "succeeded"], [gone, "TASK_STATE_FAILED", "failed"]]) {
      assert.equal((await settledAs(agent, task.id)).status.state, state);
      await assert.rejects(cancel(task.id), TaskNotCancelableError);
      const { status, body } = await rpcAs(agent, "CancelTask", { id: task.id });
      assert.equal(status, 200);
      assert.deepEqual(body.error, {
        code: -32002,
        message: `task already ${how}`,
        data: [{ "@type": "type.googleapis.com/google.rpc.ErrorInfo", reason: "TASK_NOT_CANCELABLE", domain: "a2a-protocol.org" }],
      });
      assert.equal((await getAs(agent, task.id)).result.status.state, state);
    }

    // In flight: the attempt is out and cannot be recalled. Working, and nothing says canceled.
    const inFlight = async (path) => {
      const answer = held(path);
      const task = await sendAs(agent, { url: `https://dest.example.com${path}` });
      await until(async () => delivery((await getAs(agent, task.id)).result).status === "delivering");
      const { body } = await rpcAs(agent, "CancelTask", { id: task.id });
      assert.equal(body.result.status.state, "TASK_STATE_WORKING");
      assert.equal(delivery(body.result).status, "delivering");
      assert.match(body.result.status.message.parts[0].text, /cannot be stopped/);
      assert.equal(JSON.stringify(body).includes("TASK_STATE_CANCELED"), false);
      assert.equal((await getAs(agent, task.id)).result.status.state, "TASK_STATE_WORKING");
      return { task, answer };
    };
    // The destination answers 2xx after the cancel: the delivery happened, so the task completes.
    const won = await inFlight("/a2a-cancel-won");
    won.answer(200);
    assert.equal((await settledAs(agent, won.task.id)).status.state, "TASK_STATE_COMPLETED");
    // It answers with a failure that would have been retried: the task is canceled instead.
    const lost = await inFlight("/a2a-cancel-lost");
    lost.answer(503);
    const stopped = await settledAs(agent, lost.task.id);
    assert.equal(stopped.status.state, "TASK_STATE_CANCELED");
    assert.deepEqual(delivery(stopped).attempts.map((a) => a.http_status), [503]);
    assert.equal(deliveriesTo("/a2a-cancel-lost").length, 1);

    // Without the cancel_task grant nothing is canceled, whether or not the task exists.
    const reader = await register(["a2a", "get_task", "list_tasks"], agent.client);
    const standing = await sendAs(agent, { url: "https://dest.example.com/a2a-cancel-later", delay: "1h" });
    for (const id of [standing.id, NEVER]) assert.equal((await rpcAs(reader, "CancelTask", { id })).status, 403);
    assert.equal((await getAs(reader, standing.id)).result.status.state, "TASK_STATE_SUBMITTED");
  });

  test("redrive_task is refused for a task made through A2A, so once terminal it never reopens; any other task redrives as before", async () => {
    const agent = await withProject();
    for (const path of ["/a2a-redrive", "/a2a-redrive-plain"]) replies.set(path, [404]);
    const made = await sendAs(agent, { url: "https://dest.example.com/a2a-redrive" });
    const plain = await run(agent, "queue_http", { url: "https://dest.example.com/a2a-redrive-plain" });
    const canceled = await sendAs(agent, { url: "https://dest.example.com/a2a-redrive", delay: "1h" });
    await rpcAs(agent, "CancelTask", { id: canceled.id });
    assert.equal((await settledAs(agent, made.id)).status.state, "TASK_STATE_FAILED");
    await until(async () => (await run(agent, "get_task", { task_id: plain.id })).status === "failed");

    // Over Agent Auth, the interface that can redrive: refused for the A2A tasks, whatever their end.
    for (const task of [made, canceled]) {
      await assert.rejects(run(agent, "redrive_task", { task_id: task.id }), (e) => {
        assert.equal(e.status, 409);
        assert.equal(e.code, "a2a_task_not_redrivable");
        assert.match(e.message, /made through A2A cannot be redriven/);
        return true;
      });
    }
    assert.equal((await getAs(agent, made.id)).result.status.state, "TASK_STATE_FAILED");
    assert.equal((await getAs(agent, canceled.id)).result.status.state, "TASK_STATE_CANCELED");
    assert.equal(deliveriesTo("/a2a-redrive").length, 1);

    // A task made over Agent Auth redrives, and is sent again.
    assert.equal((await run(agent, "redrive_task", { task_id: plain.id })).status, "scheduled");
    await until(async () => (await run(agent, "get_task", { task_id: plain.id })).status === "succeeded");
    assert.equal(deliveriesTo("/a2a-redrive-plain").length, 2);
  });

  test("ListTasks returns only the caller's tasks made through A2A, most recently changed first, with paging, a total and the filters; never another host's or interface's", async () => {
    const agent = await withProject();
    const stranger = await withProject();
    const client = await a2aClient(agent);
    const list = (params = {}) => client.listTasks(ListTasksRequest.fromJSON(params));

    // Nothing yet: all four fields, empty.
    assert.deepEqual((await rpcAs(agent, "ListTasks", {})).body.result, { tasks: [], nextPageToken: "", pageSize: 50, totalSize: 0 });

    const waiting = await sendAs(agent, { url: "https://dest.example.com/a2a-list", delay: "1h" });
    const dropped = await sendAs(agent, { url: "https://dest.example.com/a2a-list", delay: "1h" });
    const sent = await sendAs(agent, { url: "https://dest.example.com/a2a-list-now" });
    const completed = await settledAs(agent, sent.id);
    await rpcAs(agent, "CancelTask", { id: dropped.id });
    // In the same project over Agent Auth, and in another host's project through A2A.
    const other = await run(agent, "schedule_http", { url: "https://dest.example.com/a2a-list-other", delay: "1h" });
    const theirs = await sendAs(stranger, { url: "https://dest.example.com/a2a-list-theirs", delay: "1h" });

    // By status timestamp, newest first: the cancel is the latest change, the untouched task the oldest.
    const all = await list();
    assert.deepEqual(ids(all.tasks), [dropped.id, sent.id, waiting.id]);
    assert.deepEqual([all.totalSize, all.pageSize, all.nextPageToken], [3, 50, ""]);
    assert.deepEqual(all.tasks.map((t) => t.status.state), [TaskState.TASK_STATE_CANCELED, TaskState.TASK_STATE_COMPLETED, TaskState.TASK_STATE_SUBMITTED]);
    const times = all.tasks.map((t) => new Date(t.status.timestamp).getTime());
    assert.deepEqual(times, [...times].sort((a, b) => b - a));
    // Artifacts are left out, not sent empty, unless asked for.
    const { result: bare } = (await rpcAs(agent, "ListTasks", {})).body;
    for (const task of bare.tasks) assert.equal("artifacts" in task, false);
    const { result: full } = (await rpcAs(agent, "ListTasks", { includeArtifacts: true, historyLength: 5 })).body;
    assert.deepEqual(full.tasks.map((t) => delivery(t).status), ["canceled", "succeeded", "scheduled"]);
    assert.equal(delivery(full.tasks[1]).attempt_count, 1);
    assert.equal("history" in full.tasks[0], false);

    // Pages: the token is the cursor, every page carries the whole count, and the last has no token.
    const first = await list({ pageSize: 2 });
    assert.deepEqual([ids(first.tasks), first.totalSize, first.pageSize], [[dropped.id, sent.id], 3, 2]);
    assert.notEqual(first.nextPageToken, "");
    const last = await list({ pageSize: 2, pageToken: first.nextPageToken });
    assert.deepEqual([ids(last.tasks), last.totalSize, last.nextPageToken], [[waiting.id], 3, ""]);

    // Filters: a state, a time, both.
    for (const [status, expected] of [["TASK_STATE_SUBMITTED", [waiting.id]], ["TASK_STATE_COMPLETED", [sent.id]], ["TASK_STATE_CANCELED", [dropped.id]], ["TASK_STATE_FAILED", []], ["TASK_STATE_WORKING", []], ["TASK_STATE_INPUT_REQUIRED", []]]) {
      const page = await list({ status });
      assert.deepEqual([ids(page.tasks), page.totalSize], [expected, expected.length], status);
    }
    const since = await list({ statusTimestampAfter: completed.status.timestamp });
    assert.deepEqual([ids(since.tasks), since.totalSize], [[dropped.id, sent.id], 2]);
    assert.deepEqual(ids((await list({ statusTimestampAfter: completed.status.timestamp, status: "TASK_STATE_COMPLETED" })).tasks), [sent.id]);
    assert.equal((await list({ statusTimestampAfter: new Date(Date.now() + 60_000).toISOString() })).totalSize, 0);
    // Working is a delivery whose attempt is out (or one waiting to be retried).
    const answer = held("/a2a-list-held");
    const working = await sendAs(agent, { url: "https://dest.example.com/a2a-list-held" });
    await until(async () => delivery((await getAs(agent, working.id)).result).status === "delivering");
    assert.deepEqual(ids((await list({ status: "TASK_STATE_WORKING" })).tasks), [working.id]);
    assert.equal((await list()).tasks[0].id, working.id);
    answer(200);
    await settledAs(agent, working.id);

    // What is not honoured is refused by name, never ignored.
    for (const [params, field] of [[{ contextId: waiting.id }, "contextId"], [{ pageSize: 0 }, "pageSize"], [{ pageSize: 101 }, "pageSize"], [{ pageToken: "x" }, "pageToken"], [{ statusTimestampAfter: "soon" }, "statusTimestampAfter"], [{ status: "TASK_STATE_DONE" }, "status"]]) {
      const { error } = (await rpcAs(agent, "ListTasks", params)).body;
      assert.equal(error?.code, -32602, JSON.stringify(params));
      assert.equal(error.data[0].metadata.field, field);
    }

    // Another host lists its own task and nothing of this one's; a host with no project is told it has none.
    const strangers = await (await a2aClient(stranger)).listTasks(ListTasksRequest.fromJSON({}));
    assert.deepEqual([ids(strangers.tasks), strangers.totalSize], [[theirs.id], 1]);
    const nobody = (await rpcAs(await register(), "ListTasks", {})).body;
    assert.equal(nobody.error.code, -32000);
    assert.match(nobody.error.message, /^project_not_found/);
    // Every other interface lists the project as before: every task, newest made first, and no total.
    const overAgentAuth = await run(agent, "list_tasks");
    assert.deepEqual(ids(overAgentAuth.tasks), [working.id, other.id, sent.id, dropped.id, waiting.id]);
    assert.equal("total" in overAgentAuth, false);
    // And takes what list_tasks advertises, as before: what ListTasks adds to a list is A2A's alone.
    for (const args of [{ status: "delivering,retrying" }, { updated_since_ms: 0 }, { cursor: "1800000000000.42" }]) {
      await assert.rejects(run(agent, "list_tasks", args), { code: "invalid_request", status: 400 }, JSON.stringify(args));
    }
    // Without the list_tasks grant nothing is listed.
    const reader = await register(["a2a", "get_task"], agent.client);
    for (const params of [{}, { status: "TASK_STATE_REJECTED" }]) assert.equal((await rpcAs(reader, "ListTasks", params)).status, 403);
  });

  test("a cancel of another host's or another interface's task is answered like one of a task that never was, and cancels nothing", async () => {
    const agent = await withProject();
    const stranger = await withProject();
    const client = await a2aClient(stranger);
    const mine = await sendAs(agent, { url: "https://dest.example.com/a2a-keep", delay: "1h" });
    const finished = await sendAs(agent, { url: "https://dest.example.com/a2a-keep-done" });
    await settledAs(agent, finished.id);
    // In the stranger's own project, over Agent Auth.
    const plain = await run(stranger, "schedule_http", { url: "https://dest.example.com/a2a-keep-plain", delay: "1h" });

    const never = (await rpcAs(stranger, "CancelTask", { id: NEVER })).body;
    assert.equal(never.error.code, -32001);
    assert.deepEqual(never.error.data, [{ "@type": "type.googleapis.com/google.rpc.ErrorInfo", reason: "TASK_NOT_FOUND", domain: "a2a-protocol.org" }]);
    // Another host's task, waiting or finished (which its own host would be told is not cancelable), another
    // interface's, and ids that could be no task's.
    for (const id of [mine.id, finished.id, plain.id, "not-a-task-id", ""]) {
      assert.deepEqual((await rpcAs(stranger, "CancelTask", { id })).body, never, id);
      await assert.rejects(client.cancelTask(CancelTaskRequest.fromJSON({ id })), TaskNotFoundError, id);
    }
    assert.deepEqual((await rpcAs(await register(), "CancelTask", { id: mine.id })).body, never);

    // Nothing was canceled, and each is still its owner's to cancel.
    assert.equal((await getAs(agent, mine.id)).result.status.state, "TASK_STATE_SUBMITTED");
    assert.equal((await run(stranger, "get_task", { task_id: plain.id })).status, "scheduled");
    assert.equal((await rpcAs(agent, "CancelTask", { id: mine.id })).body.result.status.state, "TASK_STATE_CANCELED");
    assert.equal((await run(stranger, "cancel_task", { task_id: plain.id })).status, "canceled");
    // The stranger's list never had any of them.
    assert.deepEqual((await rpcAs(stranger, "ListTasks", {})).body.result, { tasks: [], nextPageToken: "", pageSize: 50, totalSize: 0 });
  });

  // ---- What a client author runs: the example client, the SDK client alone, and the staging script ----

  /** fetch as the example client takes it, each request from an address of its own. */
  const fromAnywhere = (url, init) => {
    const ip = address();
    return fetch(url, { ...init, headers: { ...Object.fromEntries(new Headers(init?.headers)), "CF-Connecting-IP": ip, "X-Forwarded-For": ip } });
  };
  /** Where the example client keeps its keys when it is not a file. */
  const memoryStore = () => {
    const kept = new Map();
    return { get: async (key) => kept.get(key) ?? null, set: async (key, value) => void kept.set(key, value), del: async (key) => void kept.delete(key) };
  };

  /** The example client run as a program. */
  const program = (args, env = {}) => promisify(execFile)(process.execPath, [join(gatewayDir, "examples/a2a-client.mjs"), ...args], { env: { ...process.env, AGENTLATER_URL: "", ...env } });

  test("the example client goes from nothing to a delivered request; restarted, it keeps its identity and finds its project and task again; it registers a new agent when its own has expired or been revoked", async () => {
    const dir = await mkdtemp(join(tmpdir(), "a2a-example-"));
    const state = join(dir, "state", "client.json");
    const lines = [];
    const start = (over = {}) =>
      runExample({ baseUrl: origin, state, payload: { url: "https://dest.example.com/a2a-example", method: "POST", body: "hello", delay: "1s" }, fetch: fromAnywhere, pollMs: 250, log: (line) => lines.push(line), ...over });
    try {
      const first = await start();
      assert.deepEqual([first.registered, first.state, first.delivery.attempt_count], [true, "TASK_STATE_COMPLETED", 1]);
      assert.equal(deliveriesTo("/a2a-example")[0].body, "hello");

      // The private keys are in the state file, which only its owner can read, and nowhere else.
      assert.equal((await stat(state)).mode & 0o777, 0o600);
      assert.equal((await stat(join(dir, "state"))).mode & 0o777, 0o700);
      assert.deepEqual(await readdir(join(dir, "state")), ["client.json"]);
      const kept = await readFile(state, "utf8");
      assert.ok(kept.includes('\\"d\\"'), "the state file holds the private keys");
      assert.equal(lines.join("\n").includes('"d"'), false);
      // The project's signing secret is returned by create_project; the client neither keeps nor prints it.
      assert.doesNotMatch(kept + lines.join("\n"), /rls_/);

      // Restarted: the same agent, and the project and the task found again, with nothing sent twice.
      const again = await start();
      assert.deepEqual([again.registered, again.agentId, again.projectId, again.taskId, again.state], [false, first.agentId, first.projectId, first.taskId, "TASK_STATE_COMPLETED"]);
      assert.ok(lines.includes(`project ${first.projectId} found again`) && lines.includes(`task ${first.taskId} found again`), lines.join("\n"));
      // Stopped after sending but before the task's id was kept: the same message returns the same task.
      const saved = JSON.parse(kept);
      delete saved["agentlater:task"];
      await writeFile(state, JSON.stringify(saved));
      assert.equal((await start()).taskId, first.taskId);
      assert.equal(deliveriesTo("/a2a-example").length, 1);
      const another = await start({ another: true });
      assert.notEqual(another.taskId, first.taskId);
      assert.equal(deliveriesTo("/a2a-example").length, 2);

      // The agent is past the 7 days an agent lives: a new one is registered under the same host
      // key, so the project and the task are the same.
      const db = await gatewayDb();
      await db.prepare('UPDATE "agent" SET "createdAt" = ? WHERE id = ?').bind(new Date(Date.now() - 8 * 86_400_000).toISOString(), first.agentId).run();
      const renewed = await start();
      assert.deepEqual([renewed.registered, renewed.projectId, renewed.taskId], [true, first.projectId, another.taskId]);
      assert.notEqual(renewed.agentId, first.agentId);
      // Revoked, or no longer known to the gateway at all: the same.
      await db.prepare('UPDATE "agent" SET status = ? WHERE id = ?').bind("revoked", renewed.agentId).run();
      const replaced = await start();
      assert.deepEqual([replaced.registered, replaced.projectId, replaced.taskId], [true, first.projectId, another.taskId]);
      await db.prepare('DELETE FROM "agent" WHERE id = ?').bind(replaced.agentId).run();
      const last = await start();
      assert.deepEqual([last.registered, last.projectId, last.taskId], [true, first.projectId, another.taskId]);
      assert.deepEqual(new Set([first.agentId, renewed.agentId, replaced.agentId, last.agentId]).size, 4);
      assert.equal((await start()).agentId, last.agentId);

      // As a program: the gateway is its argument, its state is where AGENTLATER_STATE says, and it exits 0 for a completed task.
      const { stdout } = await program([`${origin}/`], { AGENTLATER_STATE: state });
      assert.ok(stdout.includes(`task ${another.taskId} found again`) && stdout.includes("TASK_STATE_COMPLETED: 1 attempt(s), last HTTP status 200"), stdout);
      await assert.rejects(program([]), (e) => e.code === 2 && /usage/.test(e.stderr));
      await assert.rejects(promisify(execFile)(process.execPath, [join(gatewayDir, "e2e/a2a-staging.mjs"), origin], { env: { ...process.env, RECEIVER_URL: "" } }), (e) => e.code === 2 && /usage/.test(e.stderr));
      assert.equal(deliveriesTo("/a2a-example").length, 2);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("the example client stops waiting for a task at its bound, says the task goes on, and exits 3; a later run reads the same task", async () => {
    const dir = await mkdtemp(join(tmpdir(), "a2a-example-wait-"));
    const state = join(dir, "client.json");
    const lines = [];
    try {
      const started = Date.now();
      const waited = await runExample({ baseUrl: origin, state, payload: { url: "https://dest.example.com/a2a-example-wait", delay: "1h" }, fetch: fromAnywhere, pollMs: 100, waitMs: 400, log: (line) => lines.push(line) });
      assert.deepEqual([waited.ended, waited.state, waited.delivery.attempt_count], [false, "TASK_STATE_SUBMITTED", 0]);
      assert.ok(Date.now() - started < 30_000, "the run went on long after its bound");
      assert.equal(lines.at(-1), `task ${waited.taskId} is still TASK_STATE_SUBMITTED after 0.4 s. It goes on without this run; run again to read it, with AGENTLATER_WAIT_SECONDS set to wait longer`);

      // As a program: AGENTLATER_WAIT_SECONDS is the bound, and 3 is the exit code of a run that stopped waiting.
      await assert.rejects(program([origin], { AGENTLATER_STATE: state, AGENTLATER_WAIT_SECONDS: "1" }), (e) => {
        assert.equal(e.code, 3, e.stderr);
        assert.ok(e.stdout.includes(`task ${waited.taskId} found again`) && e.stdout.includes(`task ${waited.taskId} is still TASK_STATE_SUBMITTED after 1 s`), e.stdout);
        return true;
      });
      assert.equal(deliveriesTo("/a2a-example-wait").length, 0);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("SDK interoperability: the official A2A client alone sends, gets, lists and cancels; a refusal at HTTP is waited out or answered with a new agent", async () => {
    // The first request to /a2a is answered as the rate limit answers, once.
    let limited = 0;
    const session = await connectExample({
      baseUrl: origin,
      store: memoryStore(),
      fetch: (url, init) => (String(url) === a2a() && !limited++ ? Response.json({ error: "rate_limited" }, { status: 429, headers: { "Retry-After": "1" } }) : fromAnywhere(url, init)),
    });
    await session.execute("create_project");
    const client = session.a2a;

    // Send: the client asks to wait for the end (the protocol's default); the task comes back at once, in the SDK's form.
    const sent = await client.sendMessage(exampleDelivery({ url: "https://dest.example.com/a2a-sdk", capture: true }));
    assert.equal(limited, 2);
    assert.equal(typeof sent.status.state, "number");
    assert.equal(ended(sent), false);
    // Get, until it ends.
    const done = await until(async () => { const task = await client.getTask({ id: sent.id }); return ended(task) && task; });
    assert.equal(done.status.state, TaskState.TASK_STATE_COMPLETED);
    assert.deepEqual([deliveryOf(done).status, deliveryOf(done).attempts[0].http_status, deliveryOf(done).response.status], ["succeeded", 200, 200]);
    // List.
    const waiting = await client.sendMessage(exampleDelivery({ url: "https://dest.example.com/a2a-sdk-later", delay: "1h" }));
    const page = await client.listTasks(ListTasksRequest.fromJSON({ includeArtifacts: true }));
    assert.deepEqual([ids(page.tasks), page.totalSize], [[waiting.id, sent.id], 2]);
    // A listed artifact says how the delivery stands, captured response included, but not each attempt: GetTask has those.
    assert.deepEqual([deliveryOf(page.tasks[1]).status, deliveryOf(page.tasks[1]).attempt_count, deliveryOf(page.tasks[1]).response.status], ["succeeded", 1, 200]);
    assert.equal("attempts" in deliveryOf(page.tasks[1]), false);
    assert.equal((await client.listTasks(ListTasksRequest.fromJSON({}))).tasks[0].artifacts.length, 0);
    // Cancel.
    assert.equal((await client.cancelTask(CancelTaskRequest.fromJSON({ id: waiting.id }))).status.state, TaskState.TASK_STATE_CANCELED);
    await assert.rejects(client.cancelTask(CancelTaskRequest.fromJSON({ id: sent.id })), TaskNotCancelableError);

    // A request handed to the client as plain JSON, not built by fromJSON, loses its part's data: nothing is scheduled.
    const plain = { message: { messageId: randomUUID(), role: "ROLE_USER", parts: [{ data: { url: "https://dest.example.com/a2a-sdk-plain" }, mediaType: "application/json" }] } };
    await assert.rejects(client.sendMessage(plain), (e) => e instanceof RequestMalformedError && /Send one data part/.test(e.message));
    assert.equal(deliveriesTo("/a2a-sdk-plain").length, 0);

    // An agent left idle past its session is not gone: its next token is taken, and it stays the same agent.
    const before = session.agentId();
    await (await gatewayDb()).prepare('UPDATE "agent" SET "expiresAt" = ? WHERE id = ?').bind(new Date(Date.now() - 3_600_000).toISOString(), before).run();
    assert.equal((await client.getTask({ id: sent.id })).id, sent.id);
    assert.equal(session.agentId(), before);
    // The agent is revoked while the client is running: the next request registers a new one and is answered.
    await (await gatewayDb()).prepare('UPDATE "agent" SET status = ? WHERE id = ?').bind("revoked", before).run();
    assert.equal((await client.getTask({ id: sent.id })).id, sent.id);
    assert.notEqual(session.agentId(), before);
    assert.equal(deliveriesTo("/a2a-sdk").length, 1);
  });

  test("the JSON-RPC request the docs show is taken as it is, and its answer has the shape the docs show", async () => {
    const wire = JSON.parse(await readFile(join(gatewayDir, "examples/a2a-wire.json"), "utf8"));
    const agent = await withProject();
    const res = await fetch(a2a(), {
      method: "POST",
      headers: { "Content-Type": "application/json", "A2A-Version": "1.0", "CF-Connecting-IP": address(), Authorization: `Bearer ${await agent.token()}` },
      body: JSON.stringify(wire.request),
    });
    const body = await res.json();
    /** The fields of a value and their types, whatever their values. */
    const shape = (v) => (Array.isArray(v) ? v.slice(0, 1).map(shape) : v === null ? "null" : typeof v === "object" ? Object.fromEntries(Object.keys(v).sort().map((k) => [k, shape(v[k])])) : typeof v);
    assert.deepEqual(shape(body), shape(wire.response));
    const { task } = body.result;
    assert.deepEqual([task.status.state, delivery(task).status, delivery(task).run_at_ms - delivery(task).created_ms], ["TASK_STATE_SUBMITTED", "scheduled", 900_000]);
    assert.deepEqual([wire.response.result.task.status.state, delivery(wire.response.result.task).status], ["TASK_STATE_SUBMITTED", "scheduled"]);
  });

  test("the A2A staging script passes every scenario against the local stack, and says which failed when one does", async () => {
    const options = { gatewayUrl: origin, receiverUrl: `https://${RECEIVER_HOST}`, receiverReadUrl: receiverOrigin, receiverToken, fetch: fromAnywhere, pollMs: 250, timeoutMs: 60_000 };
    const { ok, report, rows } = await runA2aReport(options);
    assert.ok(ok, report);
    assert.deepEqual(
      rows.map((row) => row.name),
      [
        "A new identity registers and makes its project",
        "Immediate success",
        "Delayed success",
        "Expected HTTP failure (404, not retried)",
        "Retries exhausted (503 twice, max_attempts 2)",
        "Cancellation",
        "Duplicate submission",
        "ListTasks shows this run's tasks, latest change first",
        "A second host is refused",
        "A failed task cannot be redriven",
        "The destination saw each delivery the expected number of times",
        "Clean up: cancel what is still scheduled, revoke both agents",
      ],
    );
    assert.match(report, /Result: \*\*PASS\*\*, 12 of 12 scenarios/);
    for (const row of rows.slice(1, 7)) assert.match(report, new RegExp(`\\| ${row.name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")} \\| pass \\| [0-9.]+ \\| \`task_[0-9a-f]{20}\` \\|`));

    // A destination that answers 200 to everything: the two failures that were expected do not happen, and the report says so.
    const wrong = await runA2aReport({ ...options, receiverUrl: "https://dest.example.com/a2a-report", receiverToken: undefined });
    assert.equal(wrong.ok, false);
    assert.deepEqual(wrong.rows.filter((row) => !row.ok).map((row) => row.name), ["Expected HTTP failure (404, not retried)", "Retries exhausted (503 twice, max_attempts 2)", "A failed task cannot be redriven"]);
    assert.match(wrong.report, /Result: \*\*FAIL\*\*, 8 of 11 scenarios/);
  });

  test("the 61st request in a minute from one address is refused with 429, whichever agent sends it", async () => {
    const ip = "203.0.113.61";
    const other = await register();
    for (let i = 0; i < 60; i++) await dispatched(await call(await (i % 2 ? caller : other).token(), { ip }));
    const limited = await call(await caller.token(), { ip });
    assert.equal(limited.status, 429);
    assert.ok(Number(limited.headers.get("Retry-After")) > 0);
    assert.equal((await limited.json()).error, "rate_limited");
    await dispatched(await call(await caller.token()), "another address is not limited");
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
          fileURLToPath(import.meta.resolve("@auth/agent-cli")),
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
    assert.ok(r.html.includes(`Connect ${clientName} to AgentLater?`), r.html);
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
    const read = value(await again.callTool({ name: "get_project", arguments: {} }));
    assert.equal(read.id, project.id);
    assert.equal("signing_secret" in read, false, "the get_project tool leaves the signing secret out");
    const rotated = value(await again.callTool({ name: "rotate_signing_secret", arguments: { overlap: "0s" } }));
    assert.match(rotated.signing_secret, /^rls_[0-9a-f]{64}$/);
    assert.equal(rotated.previous_valid_until_ms, null);
    const after = value(await again.callTool({ name: "get_project", arguments: {} }));
    assert.deepEqual(["signing_secret" in after, after.signing_secret_previous_until_ms], [false, null]);
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

  test("a client with no public URL reads the result from its inbox in a later session", async () => {
    const { access_token } = await connect("Session Agent");
    const first = await mcpClient(access_token);
    value(await first.callTool({ name: "create_project", arguments: {} }));
    const task = value(
      await first.callTool({
        name: "schedule_http",
        arguments: { url: "https://dest.example.com/deploys/123", method: "GET", capture: true, callback_url: "inbox" },
      }),
    );
    await first.close();

    // Next session: the project says something is waiting, and read_inbox has it.
    const next = await mcpClient(access_token);
    await until(async () => value(await next.callTool({ name: "get_project", arguments: {} })).inbox_unread === 1);
    const page = value(await next.callTool({ name: "read_inbox", arguments: { ack: true } }));
    assert.equal(page.events.length, 1);
    const [event] = page.events;
    assert.equal(event.event, "task.finished");
    assert.equal(event.task.id, task.id);
    assert.equal(event.task.status, "succeeded");
    assert.equal(event.task.response.status, 200);
    assert.equal(page.unread, 0);
    assert.equal(value(await next.callTool({ name: "read_inbox", arguments: {} })).events.length, 0);

    const beyond = await next.callTool({ name: "ack_inbox", arguments: { up_to: event.seq + 1 } });
    assert.equal(beyond.isError, true);
    assert.equal(value(beyond).error.code, "ack_beyond_inbox");
    await next.close();
  });

  test("set_wake starts the agent when a result lands in its inbox", async () => {
    const client = await mcpClient((await connect("Woken Agent")).access_token);
    const project = value(await client.callTool({ name: "create_project", arguments: {} }));
    const wake = value(
      await client.callTool({
        name: "set_wake",
        arguments: { url: "https://dest.example.com/agent-trigger", headers: { Authorization: "Bearer trig" }, debounce: "1m" },
      }),
    );
    assert.equal(wake.state, "active");
    assert.deepEqual(wake.header_names, ["Authorization"]);
    assert.equal(value(await client.callTool({ name: "get_project", arguments: {} })).wake.url, "https://dest.example.com/agent-trigger");

    await client.callTool({ name: "schedule_http", arguments: { url: "https://dest.example.com/wake-job", callback_url: "inbox" } });
    const call = await until(() => received.find((r) => r.url === "https://dest.example.com/agent-trigger"));
    assert.equal(call.headers.authorization, "Bearer trig");
    assert.deepEqual(JSON.parse(call.body), { event: "inbox.unread", project_id: project.id, unread: 1 });

    const bad = await client.callTool({ name: "set_wake", arguments: { url: "https://dest.example.com/", debounce: "5s" } });
    assert.equal(bad.isError, true);
    assert.equal(value(bad).error.field, "debounce");
    assert.equal(value(await client.callTool({ name: "delete_wake", arguments: {} })).url, "https://dest.example.com/agent-trigger");
    const again = await client.callTool({ name: "delete_wake", arguments: {} });
    assert.equal(value(again).error.code, "wake_not_set");
    await client.close();
  });

  test("a secret set over MCP is filled in only for its host and never shown again", async () => {
    const client = await mcpClient((await connect("Secret Keeper")).access_token);
    value(await client.callTool({ name: "create_project", arguments: {} }));
    const token = `tok_${randomBytes(8).toString("hex")}`;
    const set = value(
      await client.callTool({ name: "set_secret", arguments: { name: "DEST_TOKEN", value: token, allowed_hosts: ["dest.example.com"] } }),
    );
    assert.deepEqual(set.allowed_hosts, ["dest.example.com"]);
    assert.ok(!JSON.stringify(set).includes(token));

    const task = value(
      await client.callTool({
        name: "schedule_http",
        arguments: { url: "https://dest.example.com/with-secret", headers: { Authorization: "Bearer {{secret:DEST_TOKEN}}" } },
      }),
    );
    const sent = await until(() => received.find((r) => r.headers["runlater-task-id"] === task.id));
    assert.equal(sent.headers.authorization, `Bearer ${token}`);

    const refused = await client.callTool({
      name: "schedule_http",
      arguments: { url: "https://elsewhere.example.com/", headers: { Authorization: "{{secret:DEST_TOKEN}}" } },
    });
    assert.equal(refused.isError, true);
    assert.equal(value(refused).error.code, "secret_host_not_allowed");
    const listed = value(await client.callTool({ name: "list_secrets", arguments: {} }));
    assert.deepEqual(listed.secrets.map((s) => s.name), ["DEST_TOKEN"]);
    assert.ok(!JSON.stringify(listed).includes(token));
    assert.equal(value(await client.callTool({ name: "delete_secret", arguments: { name: "DEST_TOKEN" } })).name, "DEST_TOKEN");
    await client.close();
  });

  test("a person buys attempts through the payment link; Stripe's webhook adds them once", async () => {
    const client = await mcpClient((await connect("Buyer")).access_token);
    const project = value(await client.callTool({ name: "create_project", arguments: {} }));

    const link = value(await client.callTool({ name: "get_payment_link", arguments: {} }));
    assert.equal(link.attempts, 50_000);
    assert.deepEqual(link.price, { amount_cents: 500, currency: "usd" });
    const url = new URL(link.payment_url);
    assert.equal(`${url.origin}${url.pathname}`, "https://agentlater.io/pay");
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

  test("discovery documents describe the paid MPP endpoint", async () => {
    const doc = await (await fetch(`${origin}/openapi.json`)).json();
    assert.equal(doc.servers[0].url, origin);
    assert.equal(doc.paths["/billing/mpp/{link}"].post["x-payment-info"].amount, "500");
    const llms = await fetch(`${origin}/llms.txt`);
    assert.match(llms.headers.get("content-type"), /^text\/plain/);
    assert.match(await llms.text(), new RegExp(`${origin}/mcp`));
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
    // ok + flaky (2) + gone + behind + delayed + its callback + one poll + its on_match action
    // + the captured task + its wake + the secret's delivery + the delivery after the rotation
    // = 13 attempts; canceled tasks, the deleted cron, the inbox and the refused leak use none.
    assert.equal(result.attemptsUsed, 13);
  });
});
