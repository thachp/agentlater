// The agent journey against a deployed (or local) stack: a fresh Agent Auth identity, a
// project, and real deliveries to the staging receiver, checked end to end including the
// signature, lanes, delays, callbacks, crons, watches, the inbox with captured responses,
// wake, secrets, inbound hooks, a signing-secret rotation, and hosted MCP. Used by `staging.mjs` after each staging deploy, and by agent.e2e.mjs locally.

import { createHmac, randomBytes, timingSafeEqual } from "node:crypto";
import { AgentAuthClient, MemoryStorage } from "@auth/agent";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { connectHosted } from "./oauth.mjs";

const CAPABILITIES = [
  "create_project", "get_project", "rotate_signing_secret", "schedule_http", "queue_http", "get_task", "list_tasks", "cancel_task", "redrive_task",
  "create_cron", "get_cron", "list_crons", "delete_cron", "watch_http",
  "read_inbox", "ack_inbox", "set_wake", "delete_wake",
  "set_secret", "list_secrets", "delete_secret",
  "create_hook", "list_hooks", "update_hook", "delete_hook",
];

class JourneyError extends Error {}
const check = (ok, message) => {
  if (!ok) throw new JourneyError(message);
};

/**
 * @param {object} c
 * @param {string} c.gatewayUrl   where agents connect, e.g. https://gateway-staging.agentlater.io
 * @param {string} c.receiverUrl  public https base the API delivers to
 * @param {string} [c.receiverReadUrl] base for reading recorded deliveries (defaults to receiverUrl)
 * @param {string} c.receiverToken bearer token for reading recorded deliveries
 * @param {(msg: string) => void} [c.log]
 * @param {number} [c.timeoutMs] per wait
 */
export async function runJourney(c) {
  const log = c.log ?? (() => {});
  const timeoutMs = c.timeoutMs ?? 120_000;
  const readUrl = c.receiverReadUrl ?? c.receiverUrl;
  const run = `run-${Date.now().toString(36)}-${randomBytes(4).toString("hex")}`;
  const hook = (name) => `${c.receiverUrl}/hook/${run}/${name}`;

  const until = async (what, probe) => {
    const end = Date.now() + timeoutMs;
    for (;;) {
      const value = await probe();
      if (value) return value;
      if (Date.now() > end) throw new JourneyError(`timed out waiting for ${what}`);
      // Gentle on Better Auth's per-path rate limits.
      await new Promise((r) => setTimeout(r, 2_000));
    }
  };

  log(`run ${run}`);
  const health = await fetch(`${c.gatewayUrl}/health`);
  check(health.ok, `gateway /health returned ${health.status}`);
  check((await health.json()).api === "ok", "gateway cannot reach the API");
  log("health ok");

  const client = new AgentAuthClient({
    storage: new MemoryStorage(),
    directoryUrl: null,
    allowDirectDiscovery: true,
    hostName: `AgentLater journey ${run}`,
  });
  const { agentId } = await client.connectAgent({
    provider: c.gatewayUrl,
    mode: "autonomous",
    capabilities: CAPABILITIES,
    name: "journey",
  });
  // Agent Auth allows 60 capability calls a minute; a 429 is refused before it runs, so the
  // journey waits it out and sends the same call again.
  const exec = async (capability, args = {}) => {
    for (let tries = 1; ; tries++) {
      try {
        const r = await client.executeCapability({ agentId, capability, arguments: args });
        return r.data ?? r;
      } catch (e) {
        if (e?.status !== 429 || tries >= 8) throw e;
        await new Promise((r) => setTimeout(r, 10_000));
      }
    }
  };
  log("agent registered");

  const project = await exec("create_project");
  check(/^prj_/.test(project.id) && project.signing_secret, "create_project returned no project");
  log(`project ${project.id}`);

  const deliveries = async () => {
    const res = await fetch(`${readUrl}/runs/${run}`, { headers: { Authorization: `Bearer ${c.receiverToken}` } });
    check(res.ok, `receiver read returned ${res.status}`);
    return (await res.json()).deliveries;
  };
  const settled = (task, statuses) =>
    until(`${task.id} to reach ${statuses.join("/")}`, async () => {
      const t = await exec("get_task", { task_id: task.id });
      return statuses.includes(t.status) && t;
    });
  // True only for a header with one v1= entry, made with `secret`.
  const verify = (d, body, secret = project.signing_secret) => {
    const h = d.headers;
    const mac = createHmac("sha256", secret)
      .update(`${h["runlater-timestamp"]}.${h["runlater-task-id"]}.${h["runlater-attempt"]}.`)
      .update(body)
      .digest();
    const sent = Buffer.from((h["runlater-signature"] ?? "").replace(/^v1=/, ""), "hex");
    return sent.length === mac.length && timingSafeEqual(sent, mac);
  };

  // 1. A signed delivery, verified at the destination.
  const body = JSON.stringify({ run, hello: "staging" });
  const ok = await exec("schedule_http", {
    url: hook("ok"),
    body,
    headers: { "Content-Type": "application/json", "X-Journey": run },
  });
  const okDone = await settled(ok, ["succeeded", "failed"]);
  check(okDone.status === "succeeded", `ok task ended ${okDone.status}: ${JSON.stringify(okDone.attempts)}`);
  const okDelivery = (await deliveries()).find((d) => d.headers["runlater-task-id"] === ok.id);
  check(okDelivery, "the receiver has no record of the ok delivery");
  check(okDelivery.body === body, "delivered body differs from the scheduled one");
  check(okDelivery.headers["x-journey"] === run, "custom header was not delivered");
  check(verify(okDelivery, body), "RunLater-Signature does not verify with the project's signing_secret");
  log("signed delivery verified");

  // 2. A 503 is retried with backoff and then succeeds. It holds its lane meanwhile: the
  //    task queued behind it is delivered only after the retry.
  const lane = `journey-${run}`;
  const flaky = await exec("queue_http", { url: hook("flaky"), queue: lane });
  const behind = await exec("schedule_http", { url: hook("behind"), lane });
  check(flaky.lane === lane && behind.lane === lane, "queue_http and schedule_http did not share the queue");
  const flakyDone = await settled(flaky, ["succeeded", "failed"]);
  const outcomes = flakyDone.attempts.map((a) => a.outcome);
  check(
    flakyDone.status === "succeeded" && outcomes.join() === "http_503,http_200",
    `flaky task: ${flakyDone.status} ${outcomes.join()}`,
  );
  // Waits on the receiver, not get_task: the gateway rate-limits capability calls.
  const inLane = await until("the task behind in the lane", async () => {
    const ds = (await deliveries()).filter((d) => d.name === "flaky" || d.name === "behind");
    return ds.some((d) => d.name === "behind") && ds;
  });
  const order = inLane.map((d) => d.name);
  check(order.join() === "flaky,flaky,behind", `lane delivered out of order: ${order.join()}`);
  log("retry after 503 succeeded, and its lane waited for it");

  // 3. A 404 fails without a retry.
  const gone = await exec("schedule_http", { url: hook("gone"), method: "DELETE" });
  const goneDone = await settled(gone, ["failed", "succeeded"]);
  check(goneDone.status === "failed" && goneDone.attempt_count === 1, `gone task: ${goneDone.status}, ${goneDone.attempt_count} attempts`);
  log("404 failed once, no retry");

  // 4. A future task can be canceled and is never sent.
  const later = await exec("schedule_http", { url: hook("later"), run_at_ms: Date.now() + 3_600_000 });
  const canceled = await exec("cancel_task", { task_id: later.id });
  check(canceled.status === "canceled", `cancel returned ${canceled.status}`);
  log("future task canceled");

  // 5. Idempotent scheduling and validation errors.
  const key = `journey-${run}`;
  const first = await exec("schedule_http", { url: hook("later"), run_at_ms: Date.now() + 3_600_000, idempotency_key: key });
  const again = await exec("schedule_http", { url: hook("later"), run_at_ms: first.run_at_ms, idempotency_key: key });
  check(first.id === again.id, "idempotent schedule created a second task");
  await exec("cancel_task", { task_id: first.id });
  const invalid = await exec("schedule_http", { url: "http://insecure.example.com/" }).then(
    () => null,
    (e) => e,
  );
  check(invalid?.code === "invalid_request", `insecure URL was not rejected: ${invalid?.code ?? "accepted"}`);
  log("idempotency and validation ok");

  // 6. A relative delay, then the final result posted to callback_url, signed.
  const delayed = await exec("schedule_http", { url: hook("delayed"), delay: "5s", callback_url: hook("callback") });
  check(delayed.run_at_ms - delayed.created_ms === 5_000, `delay 5s gave run_at_ms ${delayed.run_at_ms - delayed.created_ms}ms out`);
  const callback = await until("the callback", async () => (await deliveries()).find((d) => d.name === "callback"));
  const sent = (await deliveries()).find((d) => d.name === "delayed");
  check(sent && sent.received_at >= delayed.run_at_ms, "the delayed task was delivered early");
  check(verify(callback, callback.body), "the callback's RunLater-Signature does not verify");
  const report = JSON.parse(callback.body);
  check(
    report.event === "task.finished" && report.task?.id === delayed.id && report.task.status === "succeeded",
    `unexpected callback body: ${callback.body.slice(0, 200)}`,
  );
  log("delay held, signed callback received");

  // 7. A cron: created, replaced by name, read, and deleted. An hourly cron cannot fire
  //    inside one journey; firing and alerts are covered by the API's tests.
  const cronArgs = { name: "journey", schedule: "0 6 * * *", url: hook("cron"), alert_url: hook("alert") };
  const cron = await exec("create_cron", cronArgs);
  const next = new Date(cron.next_run_ms ?? 0);
  check(
    cron.health === "pending" && next.getUTCHours() === 6 && next.getUTCMinutes() === 0 && cron.next_run_ms > cron.created_ms,
    `create_cron returned ${JSON.stringify(cron)}`,
  );
  const replaced = await exec("create_cron", { ...cronArgs, schedule: "30 7 * * 1" });
  check(replaced.schedule === "30 7 * * 1" && new Date(replaced.next_run_ms).getUTCDay() === 1, "create_cron did not replace by name");
  const tooOften = await exec("create_cron", { ...cronArgs, schedule: "*/10 * * * *" }).then(
    () => null,
    (e) => e,
  );
  check(tooOften?.code === "invalid_request", `a 10-minute cron was not rejected: ${tooOften?.code ?? "accepted"}`);
  const listed = await exec("list_crons");
  check(listed.crons?.map((c) => c.name).join() === "journey", `list_crons returned ${JSON.stringify(listed)}`);
  check((await exec("get_cron", { name: "journey" })).schedule === "30 7 * * 1", "get_cron disagrees with list_crons");
  await exec("delete_cron", { name: "journey" });
  const deleted = await exec("get_cron", { name: "journey" }).then(
    () => null,
    (e) => e,
  );
  check(deleted?.code === "cron_not_found", `deleted cron still readable: ${deleted?.code ?? "found"}`);
  log("cron created, replaced, listed and deleted");

  // 8. A watch: its first poll is immediate, and a 200 satisfies { status: [200] }; the match
  //    sends its on_match request once. Longer watches (polls every 5m or more) are covered by
  //    the API's tests.
  const watch = await exec("watch_http", {
    url: hook("watch"),
    headers: { "X-Journey": run },
    every: "5m",
    until: { status: [200] },
    deadline: "1h",
    on_match: { url: hook("acted"), headers: { "X-Journey": run }, body: JSON.stringify({ run }) },
  });
  check(watch.watch?.every_ms === 300_000, `watch_http returned ${JSON.stringify(watch)}`);
  check(watch.watch?.on_match?.url === hook("acted"), `watch_http on_match: ${JSON.stringify(watch.watch?.on_match)}`);
  const polled = await until("the watch's poll", async () => (await deliveries()).find((d) => d.name === "watch"));
  check(polled.method === "GET" && polled.headers["x-journey"] === run, "the poll was not a GET with the watch's headers");
  const watched = await settled(watch, ["succeeded", "failed"]);
  check(
    watched.status === "succeeded" && watched.watch.reason === "matched" && watched.watch.matched === 200,
    `watch ended ${watched.status}: ${JSON.stringify(watched.watch)}`,
  );
  check(watched.header_names.join() === "X-Journey", "the watch's header names were not kept");
  const actionId = watched.watch.action_task_id;
  check(actionId, `the matched watch queued no action: ${JSON.stringify(watched.watch)}`);
  const acted = await until("the watch's action", async () => (await deliveries()).find((d) => d.name === "acted"));
  check(
    acted.method === "POST" && acted.headers["x-journey"] === run && JSON.parse(acted.body).run === run,
    "the action was not a POST with its own headers and body",
  );
  check(acted.headers["runlater-task-id"] === actionId, "the action arrived under another task id");
  check(verify(acted, acted.body), "the action's RunLater-Signature does not verify");
  const action = await settled({ id: actionId }, ["succeeded", "failed"]);
  check(action.status === "succeeded" && action.triggered_by === watch.id, `action ended ${action.status}, triggered_by ${action.triggered_by}`);
  log("watch matched on its first poll and sent its on_match request");

  // 9. The inbox and wake, as an agent with no public URL uses them: set a wake, schedule a
  //    task that captures its response and reports to the inbox, get woken with only a count,
  //    then read the result and ack it.
  const wake = await exec("set_wake", { url: hook("wake"), headers: { "X-Journey": run }, debounce: "1m" });
  check(wake.state === "active" && wake.header_names.join() === "X-Journey", `set_wake returned ${JSON.stringify(wake)}`);
  const captured = await exec("schedule_http", { url: hook("captured"), method: "GET", capture: true, callback_url: "inbox" });
  const woken = await until("the wake", async () => (await deliveries()).find((d) => d.name === "wake"));
  check(woken.method === "POST" && woken.headers["x-journey"] === run, "the wake was not a POST with its headers");
  check(verify(woken, woken.body), "the wake's RunLater-Signature does not verify");
  const nudge = JSON.parse(woken.body);
  check(
    nudge.event === "inbox.unread" && nudge.project_id === project.id && nudge.unread >= 1 && Object.keys(nudge).length === 3,
    `unexpected wake body: ${woken.body}`,
  );
  const waiting = await exec("get_project");
  check(waiting.inbox_unread >= 1 && waiting.wake?.last_wake_task_id, `get_project after the wake: ${JSON.stringify(waiting)}`);
  const page = await exec("read_inbox", { ack: true });
  const finished = page.events.find((e) => e.event === "task.finished" && e.task?.id === captured.id);
  check(finished, `read_inbox has no report for the captured task: ${JSON.stringify(page.events.map((e) => e.event))}`);
  check(finished.task.status === "succeeded" && finished.task.callback_task_id === null, `inbox report: ${JSON.stringify(finished.task)}`);
  const response = finished.task.response;
  check(
    response?.status === 200 && JSON.parse(response.body ?? "null")?.recorded === true && response.headers["content-type"]?.startsWith("application/json"),
    `captured response: ${JSON.stringify(response)}`,
  );
  check(page.unread === 0 && page.acked_seq === finished.seq, `read_inbox ack left ${page.unread} unread at ${page.acked_seq}`);
  const beyond = await exec("ack_inbox", { up_to: page.acked_seq + 1 }).then(
    () => null,
    (e) => e,
  );
  check(beyond?.code === "ack_beyond_inbox", `acking past the inbox was not refused: ${beyond?.code ?? "accepted"}`);
  await exec("delete_wake");
  check((await exec("get_project")).wake === null, "delete_wake left a wake set");
  log("woken with a count, read the captured result from the inbox");

  // 10. A secret: filled in only when sending, only to its host, never shown again.
  const receiverHost = new URL(c.receiverUrl).hostname;
  const secretValue = `journey-${randomBytes(12).toString("hex")}`;
  const stored = await exec("set_secret", { name: "JOURNEY_TOKEN", value: secretValue, allowed_hosts: [receiverHost] });
  check(!JSON.stringify(stored).includes(secretValue), "set_secret echoed the value");
  const withSecret = await exec("schedule_http", { url: hook("secret"), headers: { "X-Journey-Token": "{{secret:JOURNEY_TOKEN}}" } });
  const filled = await until("the delivery with a secret", async () => (await deliveries()).find((d) => d.name === "secret"));
  check(filled.headers["x-journey-token"] === secretValue, "the secret was not filled in at the destination");
  check(!JSON.stringify(await exec("get_task", { task_id: withSecret.id })).includes(secretValue), "get_task shows the secret");
  const leak = await exec("schedule_http", { url: "https://example.com/", headers: { "X-Leak": "{{secret:JOURNEY_TOKEN}}" } }).then(
    () => null,
    (e) => e,
  );
  check(leak?.code === "secret_host_not_allowed", `a secret could be sent to another host: ${leak?.code ?? "accepted"}`);
  const secretsListed = await exec("list_secrets");
  check(secretsListed.secrets?.map((s) => s.name).join() === "JOURNEY_TOKEN", `list_secrets returned ${JSON.stringify(secretsListed)}`);
  check(!JSON.stringify(secretsListed).includes(secretValue), "list_secrets shows the value");
  await exec("delete_secret", { name: "JOURNEY_TOKEN" });
  log("secret filled in for its host only, never shown");

  // 11. An inbound hook: a GitHub-style signed webhook to the hook's URL lands in the inbox
  //     once; a retry, a forged body and a GET don't add anything; deleting it retires the URL.
  const hookSecret = `journey-hook-${run}`;
  const inbound = await exec("create_hook", { name: "journey", verify: { scheme: "github", secret: hookSecret } });
  check(/\/h\/prj_[0-9a-f]{24}\/hk_[0-9a-f]{64}$/.test(inbound.url ?? ""), `create_hook returned ${JSON.stringify(inbound)}`);
  check(!JSON.stringify(inbound).includes(hookSecret), "create_hook echoed the secret");
  const hookBody = JSON.stringify({ action: "completed", run });
  const toHook = (b, delivery = `journey-${run}`) =>
    fetch(inbound.url, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-GitHub-Event": "workflow_run",
        "X-GitHub-Delivery": delivery,
        "X-Hub-Signature-256": `sha256=${createHmac("sha256", hookSecret).update(b).digest("hex")}`,
      },
      body: b,
    });
  // The hooks host is a custom domain of its own: right after the deploy that first adds it,
  // its certificate can take a moment to be issued.
  const firstHook = await until("the hooks host to answer", async () => {
    const res = await toHook(hookBody).catch(() => null);
    return res && res.status < 500 ? res : null;
  });
  // 200 too: if a first try was written but its answer was lost, the retry is a duplicate.
  check(firstHook.status === 202 || firstHook.status === 200, `the signed webhook got ${firstHook.status}`);
  check((await toHook(hookBody)).status === 200, "the webhook's retry was not recognized");
  const forgedHook = await fetch(inbound.url, { method: "POST", headers: { "X-Hub-Signature-256": "sha256=00" }, body: hookBody });
  check(forgedHook.status === 401, `a forged webhook got ${forgedHook.status}`);
  check((await fetch(inbound.url)).status === 405, "the hook URL answered a GET");
  const hookPage = await exec("read_inbox", { ack: true });
  const hookEvents = hookPage.events.filter((e) => e.event === "hook.received" && e.hook === "journey");
  check(hookEvents.length === 1, `expected one hook event, got ${hookEvents.length}`);
  check(
    hookEvents[0].verified === true && hookEvents[0].request.body === hookBody && hookEvents[0].request.headers["x-github-event"] === "workflow_run",
    `hook event: ${JSON.stringify(hookEvents[0])}`,
  );
  const hookListed = (await exec("list_hooks")).hooks?.find((h) => h.name === "journey");
  check(hookListed?.received === 1 && hookListed.refused.signature_invalid === 1, `list_hooks returned ${JSON.stringify(hookListed)}`);
  // A filter, set without changing the URL: a non-matching request is answered and dropped.
  await exec("update_hook", { name: "journey", accept: [{ path: "/action", in: ["failed"] }] });
  check((await toHook(hookBody, `journey-${run}-noise`)).status === 202, "a filtered webhook was not answered 202");
  const failedBody = JSON.stringify({ action: "failed", run });
  check((await toHook(failedBody, `journey-${run}-failed`)).status === 202, "a matching webhook was not accepted");
  const afterFilter = (await exec("read_inbox", { ack: true })).events.filter((e) => e.event === "hook.received" && e.hook === "journey");
  check(
    afterFilter.length === 1 && afterFilter[0].request.body === failedBody,
    `the filter kept ${JSON.stringify(afterFilter.map((e) => e.request.body))}`,
  );
  const filteredListed = (await exec("list_hooks")).hooks?.find((h) => h.name === "journey");
  check(filteredListed?.filtered === 1 && filteredListed.received === 2, `list_hooks after the filter: ${JSON.stringify(filteredListed)}`);
  await exec("delete_hook", { name: "journey" });
  check((await toHook(hookBody, `journey-${run}-2`)).status === 404, "a deleted hook still accepted requests");
  log("hook received a signed webhook once, refused the rest, filtered what it was told to");

  // 12. Rotating the signing secret with no overlap: the next delivery carries one signature,
  //     made with the new secret. The project was created by this run for this run, in every
  //     environment, so no destination outside it holds the secret that stops here.
  const rotated = await exec("rotate_signing_secret", { overlap: "0s" });
  check(/^rls_/.test(rotated.signing_secret ?? ""), "rotate_signing_secret returned no signing_secret");
  check(rotated.signing_secret !== project.signing_secret, "rotate_signing_secret returned the secret it replaced");
  check(rotated.previous_valid_until_ms === null, `a rotation with no overlap left the old secret until ${rotated.previous_valid_until_ms}`);
  check((await exec("create_project")).signing_secret === rotated.signing_secret, "create_project does not return the rotated secret");
  await exec("schedule_http", { url: hook("rotated"), body });
  const afterRotation = await until("the delivery after the rotation", async () => (await deliveries()).find((d) => d.name === "rotated"));
  check(verify(afterRotation, body, rotated.signing_secret), "the delivery after the rotation does not verify with the new signing_secret alone");
  log("signing secret rotated with no overlap, next delivery signed with the new one only");

  // 13. Hosted MCP: a client connects through OAuth consent (its own project and trial) and
  //    schedules a delivery with the schedule_http tool.
  const tokens = await connectHosted(c.gatewayUrl);
  const mcp = new Client({ name: "runlater-journey", version: "1.0.0" });
  await mcp.connect(
    new StreamableHTTPClientTransport(new URL(`${c.gatewayUrl}/mcp`), {
      requestInit: { headers: { Authorization: `Bearer ${tokens.access_token}` } },
    }),
  );
  try {
    const tool = async (name, args = {}) => {
      const r = await mcp.callTool({ name, arguments: args });
      check(!r.isError, `${name} over MCP failed: ${r.content?.[0]?.text}`);
      return JSON.parse(r.content[0].text);
    };
    const names = (await mcp.listTools()).tools.map((t) => t.name);
    check(CAPABILITIES.every((n) => names.includes(n)), `MCP tools were ${names.join(", ")}`);
    const mcpProject = await tool("create_project");
    check(mcpProject.id !== project.id, "a hosted MCP connection shared the agent's project");
    const viaMcp = await tool("schedule_http", { url: hook("mcp"), body: run });
    await until("the hosted MCP delivery", async () => (await deliveries()).find((d) => d.name === "mcp"));
    await until("the hosted MCP task to succeed", async () => (await tool("get_task", { task_id: viaMcp.id })).status === "succeeded");
  } finally {
    await mcp.close();
  }
  log("hosted MCP connected and delivered");

  const all = await deliveries();
  check(!all.some((d) => d.name === "later"), "a canceled task was delivered");
  check(!all.some((d) => d.name === "cron" || d.name === "alert"), "the deleted cron sent something");
  check(all.filter((d) => d.name === "wake").length === 1, "one inbox event should send exactly one wake");
  check(all.filter((d) => d.name === "acted").length === 1, "a watch's match should send its action exactly once");
  const used = (await exec("get_project")).attempts_used;
  log(`done: ${used} delivery attempts used`);
  return { run, projectId: project.id, attemptsUsed: used };
}
