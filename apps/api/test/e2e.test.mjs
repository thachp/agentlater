// End-to-end: the built Worker in local workerd (Miniflare) with real Durable Object
// SQLite, alarms and Queues. Outbound deliveries are captured by `outboundService`
// instead of leaving the machine. Run `npm run build` first.
//
// `global_fetch_strictly_public` is left off here: it would route fetch() to the real
// internet, around the capture. It is a provider guarantee, set in wrangler.jsonc.

import { after, before, describe, test } from "node:test";
import assert from "node:assert/strict";
import { createHmac, generateKeyPairSync, randomBytes, sign } from "node:crypto";
import { fileURLToPath } from "node:url";
import { Miniflare, convertV4MiniflareOptions } from "miniflare";

const root = fileURLToPath(new URL("..", import.meta.url));
const { publicKey, privateKey } = generateKeyPairSync("ed25519");
const publicHex = Buffer.from(publicKey.export({ format: "jwk" }).x, "base64url").toString("hex");

function assertion(project, scope = "project schedule read cancel") {
  const now = Math.floor(Date.now() / 1000);
  const claims = { aud: "runlater-api", sub: project, scope, iat: now, exp: now + 60 };
  const payload = Buffer.from(JSON.stringify(claims)).toString("base64url");
  const sig = sign(null, Buffer.from(payload), privateKey).toString("base64url");
  return `${payload}.${sig}`;
}

/** Requests the Worker sent out, and scripted replies per path: a status, or
 * { status, body, headers, stall } (default 200). */
const received = [];
const replies = new Map();

let mf;
before(async () => {
  // Miniflare 5 takes a new options shape; it converts from the documented v4 one.
  mf = new Miniflare(convertV4MiniflareOptions({
    modulesRoot: root,
    modules: [
      { type: "ESModule", path: `${root}build/index.js` },
      { type: "CompiledWasm", path: `${root}build/index_bg.wasm` },
    ],
    compatibilityDate: "2026-10-27",
    durableObjects: { LEDGER: { className: "TaskLedger", useSQLite: true } },
    queueProducers: { READY: { queueName: "runlater-ready" } },
    queueConsumers: { "runlater-ready": { maxBatchTimeout: 1 } },
    bindings: { GATEWAY_PUBLIC_KEY: publicHex, SECRETS_KEY: randomBytes(32).toString("hex") },
    outboundService: async (req) => {
      const url = new URL(req.url);
      received.push({
        url: req.url,
        method: req.method,
        headers: Object.fromEntries(req.headers),
        body: await req.text(),
      });
      const script = replies.get(url.pathname) ?? [];
      const reply = script.length ? script.shift() : 200;
      const { status, body, headers, stall } = typeof reply === "number" ? { status: reply, body: `status ${reply}` } : reply;
      // stall: send the status and the start of the body, then never finish it.
      if (stall) {
        const start = (c) => c.enqueue(new TextEncoder().encode(body ?? ""));
        return new Response(new ReadableStream({ start }), { status, headers });
      }
      return new Response(body, { status, headers });
    },
  }));
  await mf.ready;
});
after(() => mf?.dispose());

async function api(method, path, { project = "proj_a", body, scope, token } = {}) {
  const headers = { "Content-Type": "application/json" };
  if (token !== null) headers["RunLater-Assertion"] = token ?? assertion(project, scope);
  const res = await mf.dispatchFetch(`https://api.internal${path}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return { status: res.status, json: await res.json() };
}

async function until(check, timeoutMs = 20_000) {
  const end = Date.now() + timeoutMs;
  for (;;) {
    const value = await check();
    if (value) return value;
    if (Date.now() > end) throw new Error("timed out waiting");
    await new Promise((r) => setTimeout(r, 200));
  }
}

const settled = (project, id, statuses, timeoutMs) =>
  until(async () => {
    const { json } = await api("GET", `/v1/tasks/${id}`, { project });
    return statuses.includes(json.status) && json;
  }, timeoutMs);

let secret;

describe("api", () => {
  test("health needs no auth", async () => {
    const res = await mf.dispatchFetch("https://api.internal/health");
    assert.equal(res.status, 200);
  });

  test("rejects missing, forged and under-scoped assertions", async () => {
    assert.equal((await api("GET", "/v1/projects/self", { token: null })).status, 401);
    assert.equal((await api("GET", "/v1/projects/self", { token: "a.b" })).status, 401);
    const res = await api("POST", "/v1/tasks", { scope: "read", body: {} });
    assert.equal(res.status, 403);
    assert.equal((await api("GET", "/v1/nope")).status, 404);
  });

  test("creates a project once", async () => {
    assert.equal((await api("GET", "/v1/tasks")).json.error.code, "project_not_found");
    const first = await api("POST", "/v1/projects");
    assert.equal(first.status, 201);
    assert.match(first.json.signing_secret, /^rls_[0-9a-f]{64}$/);
    assert.equal(first.json.attempts_remaining, 1_000);
    const again = await api("POST", "/v1/projects");
    assert.equal(again.status, 200);
    assert.equal(again.json.signing_secret, first.json.signing_secret);
    secret = first.json.signing_secret;
  });

  test("validates requests and names the field", async () => {
    const res = await api("POST", "/v1/tasks", { body: { url: "http://insecure.example.com/" } });
    assert.equal(res.status, 400);
    assert.equal(res.json.error.field, "url");
    const bad = await api("POST", "/v1/tasks", { body: { url: "https://a.example.com/", nope: 1 } });
    assert.equal(bad.status, 400);
  });

  test("delivers a signed request and records the attempt", async () => {
    const body = JSON.stringify({ hello: "world" });
    const res = await api("POST", "/v1/tasks", {
      body: {
        url: "https://receiver.example.com/ok",
        headers: { "X-Custom": "yes", "Content-Type": "application/json" },
        body,
      },
    });
    assert.equal(res.status, 201);
    const id = res.json.id;
    assert.match(id, /^task_[0-9a-f]{20}$/);

    const task = await settled("proj_a", id, ["succeeded", "failed"]);
    assert.equal(task.status, "succeeded");
    assert.deepEqual(
      task.attempts.map((a) => [a.number, a.outcome, a.http_status]),
      [[1, "http_200", 200]],
    );

    const sent = received.find((r) => r.headers["runlater-task-id"] === id);
    assert.equal(sent.method, "POST");
    assert.equal(sent.body, body);
    assert.equal(sent.headers["x-custom"], "yes");
    assert.equal(sent.headers["runlater-attempt"], "1");
    const ts = sent.headers["runlater-timestamp"];
    const expected = createHmac("sha256", secret).update(`${ts}.${id}.1.${body}`).digest("hex");
    assert.equal(sent.headers["runlater-signature"], `v1=${expected}`);

    const project = await api("GET", "/v1/projects/self");
    assert.equal(project.json.attempts_used, 1);
  });

  test("retries a 503 with backoff, then succeeds", async () => {
    replies.set("/flaky", [503]);
    const { json } = await api("POST", "/v1/tasks", { body: { url: "https://receiver.example.com/flaky" } });
    const task = await settled("proj_a", json.id, ["succeeded", "failed"], 30_000);
    assert.equal(task.status, "succeeded");
    assert.deepEqual(task.attempts.map((a) => a.outcome), ["http_503", "http_200"]);
    assert.ok(task.attempts[1].started_ms - task.attempts[0].finished_ms >= 5_000);
  });

  test("fails on a 404 without retrying, and redrive tries again", async () => {
    replies.set("/gone", [404]);
    const { json } = await api("POST", "/v1/tasks", {
      body: { url: "https://receiver.example.com/gone", method: "DELETE" },
    });
    const failed = await settled("proj_a", json.id, ["failed"]);
    assert.equal(failed.attempt_count, 1);

    const redriven = await api("POST", `/v1/tasks/${json.id}/redrive`);
    assert.equal(redriven.status, 200);
    const done = await settled("proj_a", json.id, ["succeeded"]);
    assert.deepEqual(done.attempts.map((a) => a.outcome), ["http_404", "http_200"]);
  });

  test("cancels a future task before it runs", async () => {
    const { json } = await api("POST", "/v1/tasks", {
      body: { url: "https://receiver.example.com/later", run_at_ms: Date.now() + 3_600_000 },
    });
    const canceled = await api("POST", `/v1/tasks/${json.id}/cancel`);
    assert.equal(canceled.json.status, "canceled");
    assert.equal((await api("POST", `/v1/tasks/${json.id}/cancel`)).status, 200);
    assert.ok(!received.some((r) => r.url.endsWith("/later")));
  });

  test("replays an idempotent schedule", async () => {
    const body = { url: "https://receiver.example.com/once", idempotency_key: "order-42" };
    const first = await api("POST", "/v1/tasks", { body });
    const again = await api("POST", "/v1/tasks", { body });
    assert.deepEqual([first.status, again.status], [201, 200]);
    assert.equal(again.json.id, first.json.id);
    const clash = await api("POST", "/v1/tasks", { body: { ...body, method: "PUT" } });
    assert.equal(clash.json.error.code, "idempotency_conflict");
  });

  test("schedules after a relative delay", async () => {
    const before = Date.now();
    const { status, json } = await api("POST", "/v1/tasks", {
      body: { url: "https://receiver.example.com/delayed", delay: "1h" },
    });
    assert.equal(status, 201);
    assert.ok(json.run_at_ms >= before + 3_600_000 && json.run_at_ms <= Date.now() + 3_600_000);
    await api("POST", `/v1/tasks/${json.id}/cancel`);
    const both = await api("POST", "/v1/tasks", {
      body: { url: "https://receiver.example.com/delayed", delay: "1h", run_at_ms: before },
    });
    assert.equal(both.json.error.field, "delay");
  });

  test("posts the final result to callback_url, signed", async () => {
    replies.set("/job", [404]);
    const { json } = await api("POST", "/v1/tasks", {
      body: { url: "https://receiver.example.com/job", callback_url: "https://receiver.example.com/callback" },
    });
    assert.equal(json.callback_url, "https://receiver.example.com/callback");
    const failed = await settled("proj_a", json.id, ["failed"]);
    assert.match(failed.callback_task_id, /^task_[0-9a-f]{20}$/);

    const cb = await settled("proj_a", failed.callback_task_id, ["succeeded"]);
    assert.equal(cb.callback_for, json.id);
    const sent = received.find((r) => r.headers["runlater-task-id"] === cb.id);
    assert.equal(sent.method, "POST");
    assert.equal(sent.headers["content-type"], "application/json");
    const ts = sent.headers["runlater-timestamp"];
    const expected = createHmac("sha256", secret).update(`${ts}.${cb.id}.1.${sent.body}`).digest("hex");
    assert.equal(sent.headers["runlater-signature"], `v1=${expected}`);
    const report = JSON.parse(sent.body);
    assert.equal(report.event, "task.finished");
    assert.equal(report.task.id, json.id);
    assert.equal(report.task.status, "failed");
    assert.deepEqual(report.task.attempts.map((a) => a.http_status), [404]);
  });

  test("captures the response and reports to the inbox", async () => {
    replies.set("/job", [{ status: 200, body: JSON.stringify({ status: "failed", step: "migrate" }), headers: { "Content-Type": "application/json", "Set-Cookie": "s=1" } }]);
    const before = (await api("GET", "/v1/projects/self")).json.inbox_unread;
    const res = await api("POST", "/v1/tasks", {
      body: { url: "https://receiver.example.com/job", method: "GET", capture: true, callback_url: "inbox" },
    });
    assert.equal(res.status, 201);
    const task = await settled("proj_a", res.json.id, ["succeeded", "failed"]);
    assert.equal(task.status, "succeeded");
    assert.equal(task.callback_task_id, null);
    assert.equal(task.response.status, 200);
    assert.equal(JSON.parse(task.response.body).step, "migrate");
    assert.deepEqual(task.response.headers, { "content-type": "application/json" });

    assert.equal((await api("GET", "/v1/projects/self")).json.inbox_unread, before + 1);
    const page = (await api("POST", "/v1/inbox/read", { body: { ack: true }, scope: "read" })).json;
    const event = page.events.find((e) => e.task?.id === task.id);
    assert.equal(event.event, "task.finished");
    assert.equal(event.seq, task.inbox_seq);
    assert.equal(event.task.response.status, 200);
    assert.equal(page.unread, 0);
    const beyond = await api("POST", "/v1/inbox/ack", { body: { up_to: page.acked_seq + 1 }, scope: "read" });
    assert.equal(beyond.json.error.code, "ack_beyond_inbox");
  });

  test("a captured body that never finishes leaves the outcome alone", async () => {
    replies.set("/stalls", [{ status: 200, body: "partial", headers: { "Content-Type": "text/plain" }, stall: true }]);
    const res = await api("POST", "/v1/tasks", { body: { url: "https://receiver.example.com/stalls", capture: true } });
    const task = await settled("proj_a", res.json.id, ["succeeded", "failed", "retrying"], 30_000);
    assert.equal(task.status, "succeeded");
    assert.equal(task.attempt_count, 1, "no retry, so no second delivery");
    assert.equal(task.attempts[0].outcome, "http_200");
    assert.deepEqual(
      { status: task.response.status, body: task.response.body, truncated: task.response.truncated },
      { status: 200, body: null, truncated: true },
    );
    assert.equal(task.response.headers["content-type"], "text/plain");
  });

  test("wakes the agent when its inbox gets something, signed and with only a count", async () => {
    const project = "proj_wake";
    assert.equal((await api("POST", "/v1/projects", { project })).status, 201);
    const { signing_secret } = (await api("GET", "/v1/projects/self", { project })).json;
    const set = await api("POST", "/v1/wake", {
      project,
      body: { url: "https://receiver.example.com/wake", headers: { Authorization: "Bearer trig" }, debounce: "1m" },
    });
    assert.equal(set.status, 200);
    assert.equal(set.json.state, "active");
    assert.deepEqual(set.json.header_names, ["Authorization"]);

    const res = await api("POST", "/v1/tasks", { project, body: { url: "https://receiver.example.com/job", callback_url: "inbox" } });
    await settled(project, res.json.id, ["succeeded"]);
    const wake = await until(() => received.find((r) => r.url === "https://receiver.example.com/wake"));
    assert.equal(wake.method, "POST");
    assert.equal(wake.headers.authorization, "Bearer trig");
    assert.deepEqual(JSON.parse(wake.body), { event: "inbox.unread", project_id: project, unread: 1 });
    const ts = wake.headers["runlater-timestamp"];
    const id = wake.headers["runlater-task-id"];
    const mac = createHmac("sha256", signing_secret).update(`${ts}.${id}.1.${wake.body}`).digest("hex");
    assert.equal(wake.headers["runlater-signature"], `v1=${mac}`);

    const after = (await api("GET", "/v1/projects/self", { project })).json.wake;
    assert.equal(after.last_wake_task_id, id);
    assert.equal((await api("POST", "/v1/wake/delete", { project, body: {} })).json.url, "https://receiver.example.com/wake");
    assert.equal((await api("GET", "/v1/projects/self", { project })).json.wake, null);
  });

  test("fills a secret in only when sending, only to its host, and redacts it from the capture", async () => {
    const project = "proj_secrets";
    await api("POST", "/v1/projects", { project });
    const token = `tok_${randomBytes(8).toString("hex")}`;
    const set = await api("POST", "/v1/secrets", {
      project,
      scope: "secrets",
      body: { name: "RECEIVER_TOKEN", value: token, allowed_hosts: ["receiver.example.com"] },
    });
    assert.equal(set.status, 201);
    assert.deepEqual(Object.keys(set.json).sort(), ["allowed_hosts", "created_ms", "name", "updated_ms"]);
    const needsScope = await api("POST", "/v1/secrets", { project, body: { name: "X", value: "y", allowed_hosts: ["a.example.com"] } });
    assert.equal(needsScope.status, 403, "the secrets scope is separate");

    replies.set("/echo", [{ status: 200, body: JSON.stringify({ you_sent: token }), headers: { "Content-Type": "application/json" } }]);
    const res = await api("POST", "/v1/tasks", {
      project,
      body: { url: "https://receiver.example.com/echo", headers: { Authorization: "Bearer {{secret:RECEIVER_TOKEN}}" }, capture: true },
    });
    assert.equal(res.status, 201);
    const task = await settled(project, res.json.id, ["succeeded", "failed"]);
    assert.equal(task.status, "succeeded");
    const sent = received.find((r) => r.headers["runlater-task-id"] === task.id);
    assert.equal(sent.headers.authorization, `Bearer ${token}`, "the destination gets the real value");
    assert.equal(task.response.body, JSON.stringify({ you_sent: "[secret:RECEIVER_TOKEN]" }));
    assert.ok(!JSON.stringify(task).includes(token), "the task never shows it");

    const elsewhere = await api("POST", "/v1/tasks", {
      project,
      body: { url: "https://evil.example.com/", headers: { Authorization: "{{secret:RECEIVER_TOKEN}}" } },
    });
    assert.equal(elsewhere.json.error.code, "secret_host_not_allowed");
    const listed = await api("GET", "/v1/secrets", { project });
    assert.deepEqual(listed.json.secrets.map((s) => s.name), ["RECEIVER_TOKEN"]);
    assert.ok(!JSON.stringify(listed.json).includes(token));
    // An echo that straddles the 16 KiB capture cap is redacted whole, not cut to a prefix.
    const padding = "p".repeat(16 * 1024 - 10);
    replies.set("/straddle", [{ status: 200, body: `${padding}${token}tail` }]);
    const straddle = await api("POST", "/v1/tasks", {
      project,
      body: { url: "https://receiver.example.com/straddle", headers: { Authorization: "{{secret:RECEIVER_TOKEN}}" }, capture: true },
    });
    const cut = await settled(project, straddle.json.id, ["succeeded", "failed"]);
    assert.equal(cut.response.truncated, true);
    assert.ok(!cut.response.body.includes(token.slice(0, 8)), "no prefix of the value survives the cut");
    assert.ok(cut.response.body.endsWith("[secret:RE") || cut.response.body.length <= 16 * 1024);
    const short = await api("POST", "/v1/secrets", {
      project,
      scope: "secrets",
      body: { name: "PIN", value: "1234", allowed_hosts: ["receiver.example.com"] },
    });
    assert.equal(short.json.error.field, "value", "short secrets are refused");
    const wildcard = await api("POST", "/v1/secrets", {
      project,
      scope: "secrets",
      body: { name: "WILD", value: "x".repeat(20), allowed_hosts: ["*.workers.dev"] },
    });
    assert.equal(wildcard.json.error.field, "allowed_hosts", "no wildcards");
    const gone = await api("POST", "/v1/secrets/RECEIVER_TOKEN/delete", { project, scope: "secrets", body: {} });
    assert.equal(gone.json.name, "RECEIVER_TOKEN");
  });

  test("runs a lane one task at a time, in order", async () => {
    replies.set("/lane-1", [503]);
    const ids = [];
    for (const n of [1, 2, 3]) {
      const { json } = await api("POST", "/v1/tasks", {
        body: { url: `https://receiver.example.com/lane-${n}`, lane: "payments" },
      });
      assert.equal(json.lane, "payments");
      ids.push(json.id);
    }
    for (const id of ids) await settled("proj_a", id, ["succeeded"], 40_000);
    const hits = received.filter((r) => r.url.includes("/lane-")).map((r) => new URL(r.url).pathname);
    assert.deepEqual(hits, ["/lane-1", "/lane-1", "/lane-2", "/lane-3"], "lane-1's retry finishes before lane-2 starts");
    const page = await api("GET", "/v1/tasks?lane=payments");
    assert.deepEqual(page.json.tasks.map((t) => t.id), [...ids].reverse());
  });

  test("stores crons and keeps the alarm healthy", async () => {
    const body = { name: "nightly", schedule: "30 2 * * *", url: "https://receiver.example.com/nightly" };
    const created = await api("POST", "/v1/crons", { body });
    assert.equal(created.status, 201);
    assert.equal(new Date(created.json.next_run_ms).getUTCHours(), 2);
    const replaced = await api("POST", "/v1/crons", { body: { ...body, schedule: "0 3 * * *" } });
    assert.equal(replaced.status, 200);
    assert.equal(replaced.json.schedule, "0 3 * * *");
    assert.equal((await api("POST", "/v1/crons", { body: { ...body, schedule: "*/10 * * * *" } })).json.error.field, "schedule");
    assert.equal((await api("POST", "/v1/crons", { scope: "read", body })).status, 403);
    assert.deepEqual((await api("GET", "/v1/crons")).json.crons.map((c) => c.name), ["nightly"]);

    // The ledger still delivers with a cron armed (the alarm also wakes for next_run_ms).
    const { json } = await api("POST", "/v1/tasks", { body: { url: "https://receiver.example.com/with-cron" } });
    await settled("proj_a", json.id, ["succeeded"]);

    assert.equal((await api("POST", "/v1/crons/nightly/delete", { scope: "cancel" })).status, 200);
    assert.equal((await api("GET", "/v1/crons/nightly")).json.error.code, "cron_not_found");
  });

  test("watches a URL until its JSON matches, then calls back", async () => {
    replies.set("/job-status", [{ status: 200, body: JSON.stringify({ state: "done", output: "s3://out" }) }]);
    const { status, json } = await api("POST", "/v1/watches", {
      body: {
        url: "https://receiver.example.com/job-status",
        headers: { Authorization: "Bearer job-token" },
        every: "5m",
        until: { path: "/state", in: ["done", "error"] },
        deadline: "1h",
        callback_url: "https://receiver.example.com/watched",
      },
    });
    assert.equal(status, 201);
    assert.equal(json.method, "GET");
    assert.equal(json.watch.every_ms, 300_000);
    const done = await settled("proj_a", json.id, ["succeeded", "failed"]);
    assert.equal(done.status, "succeeded");
    assert.equal(done.watch.reason, "matched");
    assert.equal(done.watch.matched, "done");
    const poll = received.find((r) => r.headers["runlater-task-id"] === json.id);
    assert.equal(poll.method, "GET");
    assert.equal(poll.headers.authorization, "Bearer job-token");

    const cb = await settled("proj_a", done.callback_task_id, ["succeeded"]);
    const report = JSON.parse(received.find((r) => r.headers["runlater-task-id"] === cb.id).body);
    assert.equal(report.task.watch.matched, "done");
    assert.equal(JSON.parse(report.task.watch.last_body).output, "s3://out");
  });

  test("a watch that matches sends its on_match request once", async () => {
    replies.set("/deploy-status", [{ status: 200, body: JSON.stringify({ status: "SUCCEEDED" }) }]);
    const { status, json } = await api("POST", "/v1/watches", {
      body: {
        url: "https://receiver.example.com/deploy-status",
        every: "5m",
        until: { path: "/status", in: ["SUCCEEDED"] },
        deadline: "1h",
        on_match: {
          url: "https://receiver.example.com/smoke-tests",
          headers: { Authorization: "Bearer ci-token" },
          body: '{"deploymentId":"123"}',
          callback_url: "inbox",
        },
      },
    });
    assert.equal(status, 201, JSON.stringify(json));
    assert.equal(json.watch.on_match.url, "https://receiver.example.com/smoke-tests");
    assert.deepEqual(json.watch.on_match.header_names, ["Authorization"]);
    const done = await settled("proj_a", json.id, ["succeeded", "failed"]);
    assert.equal(done.watch.reason, "matched");
    const actionId = done.watch.action_task_id;
    assert.match(actionId, /^task_[0-9a-f]{20}$/);
    const action = await settled("proj_a", actionId, ["succeeded", "failed"]);
    assert.equal(action.status, "succeeded");
    assert.equal(action.triggered_by, json.id);
    const sent = received.filter((r) => r.headers["runlater-task-id"] === actionId);
    assert.equal(sent.length, 1);
    assert.equal(sent[0].method, "POST");
    assert.equal(sent[0].headers.authorization, "Bearer ci-token");
    assert.equal(sent[0].body, '{"deploymentId":"123"}');

    const bad = await api("POST", "/v1/watches", {
      body: { url: "https://receiver.example.com/x", every: "5m", until: { status: [200] }, deadline: "1h", on_match: { url: "http://receiver.example.com/" } },
    });
    assert.equal(bad.status, 400);
    assert.equal(bad.json.error.field, "on_match");
  });

  test("a watch ends when the body it needs is over 64 KiB", async () => {
    const big = JSON.stringify({ state: "done", log: "x".repeat(100 * 1024) });
    replies.set("/big-job", [{ status: 200, body: big }]);
    const { json } = await api("POST", "/v1/watches", {
      body: { url: "https://receiver.example.com/big-job", every: "5m", until: { path: "/state", in: ["done"] }, deadline: "1h" },
    });
    const done = await settled("proj_a", json.id, ["succeeded", "failed"]);
    assert.equal(done.status, "failed");
    assert.equal(done.watch.reason, "response_too_large");
    assert.equal(done.watch.last_body.length, 16 * 1024);
  });

  test("a watch that is not done yet waits for its next poll", async () => {
    replies.set("/slow-job", [{ status: 200, body: '{"state":"running"}' }]);
    const before = Date.now();
    const { json } = await api("POST", "/v1/watches", {
      body: { url: "https://receiver.example.com/slow-job", every: "5m", until: { status: [204] }, deadline: "1h" },
    });
    const waiting = await until(async () => {
      const t = (await api("GET", `/v1/tasks/${json.id}`)).json;
      return t.attempt_count === 1 && t.status === "scheduled" && t;
    });
    assert.ok(waiting.next_attempt_ms >= before + 300_000);
    assert.equal(waiting.watch.last_status, 200);
    assert.equal((await api("POST", `/v1/tasks/${json.id}/cancel`, { scope: "cancel" })).json.status, "canceled");
    assert.equal((await api("POST", "/v1/watches", { body: { url: "https://receiver.example.com/x", every: "1m", until: { status: [200] }, deadline: "1h" } })).json.error.field, "every");
  });

  test("a hook's requests reach the inbox only through the gateway's scope", async () => {
    const created = await api("POST", "/v1/hooks", { scope: "schedule", body: { name: "form", keep_headers: ["X-Form"] } });
    assert.equal(created.status, 201);
    const token = created.json.url_path.split("/").pop();
    assert.match(token, /^hk_[0-9a-f]{64}$/);
    const delivery = { token, headers: [["Content-Type", "text/plain"], ["X-Form", "signup"], ["Cookie", "s=1"]], body: Buffer.from("hello").toString("base64") };
    // An agent's scopes can't hand the ledger a request; only the gateway's hook route can.
    assert.equal((await api("POST", "/v1/hooks/_receive", { scope: "schedule", body: delivery })).status, 403);
    const res = await api("POST", "/v1/hooks/_receive", { scope: "hooks.receive", body: delivery });
    assert.equal(res.status, 202);
    assert.deepEqual(res.json, {});
    assert.equal((await api("POST", "/v1/hooks/_receive", { scope: "hooks.receive", body: { ...delivery, token: `hk_${"0".repeat(64)}` } })).status, 404);
    const page = (await api("POST", "/v1/inbox/read", { scope: "read", body: { limit: 50 } })).json;
    const event = page.events.find((e) => e.event === "hook.received" && e.hook === "form");
    assert.equal(event.verified, false);
    assert.equal(event.request.body, "hello");
    assert.deepEqual(event.request.headers, { "content-type": "text/plain", "x-form": "signup" });
    const listed = (await api("GET", "/v1/hooks", { scope: "read" })).json.hooks;
    assert.equal(listed[0].received, 1);
    assert.equal(JSON.stringify(listed).includes(token), false);
  });

  test("lists tasks newest first with a cursor", async () => {
    const page = await api("GET", "/v1/tasks?limit=2");
    assert.equal(page.json.tasks.length, 2);
    assert.ok(page.json.next_cursor);
    const next = await api("GET", `/v1/tasks?limit=2&cursor=${page.json.next_cursor}`);
    assert.notEqual(next.json.tasks[0].id, page.json.tasks[0].id);
    const canceled = await api("GET", "/v1/tasks?status=canceled");
    assert.ok(canceled.json.tasks.every((t) => t.status === "canceled"));
  });

  test("keeps projects apart", async () => {
    const mine = await api("GET", "/v1/tasks");
    const id = mine.json.tasks[0].id;
    await api("POST", "/v1/projects", { project: "proj_b" });
    const theirs = await api("GET", `/v1/tasks/${id}`, { project: "proj_b" });
    assert.equal(theirs.status, 404);
    const list = await api("GET", "/v1/tasks", { project: "proj_b" });
    assert.deepEqual(list.json.tasks, []);
  });
});
