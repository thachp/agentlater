#!/usr/bin/env node
// The A2A end-to-end run against a deployed gateway whose A2A interface is switched on: a new
// Agent Auth identity and project, real deliveries to the staging receiver through the official
// A2A client, and a second identity that must be refused. It prints a Markdown report to paste
// into a pull request and exits 1 if any scenario failed. agent.e2e.mjs runs it against the local stack.
//
//   node apps/gateway/e2e/a2a-staging.mjs https://gateway-staging.agentlater.io https://runlater-receiver-staging.<account>.workers.dev
//
// The two URLs can also be GATEWAY_URL and RECEIVER_URL. With RECEIVER_TOKEN set, the run also
// reads what the receiver recorded and checks that each delivery arrived the expected number of
// times. It takes 30 to 60 seconds: the longest wait is the one retry of the exhausted delivery
// (5 to 10 seconds), which max_attempts: 2 keeps from running the whole retry schedule.
//
// Left behind: two hosts and their projects (their trials end after 7 days),
// and six tasks, deleted 30 days after they finished. Like journey.mjs, it revokes its agents at the end.

import { randomBytes } from "node:crypto";
import { pathToFileURL } from "node:url";
import { CancelTaskRequest, ListTasksRequest, TaskState, taskStateToJSON } from "@a2a-js/sdk";
import { RequestMalformedError, TaskNotCancelableError, TaskNotFoundError } from "@a2a-js/sdk/errors";
import { CAPABILITIES, connect, delivery, deliveryOf, ended } from "../examples/a2a-client.mjs";

const check = (ok, message) => {
  if (!ok) throw new Error(message);
};
/** The error a promise rejects with, which must be of this class. */
async function refusal(promise, kind) {
  const error = await promise.then(
    () => null,
    (e) => e,
  );
  check(error instanceof kind, `expected ${kind.name}, got ${error ? `${error.constructor.name}: ${error.message}` : "an answer"}`);
  return error;
}
const memory = () => {
  const kept = new Map();
  return { get: async (key) => kept.get(key) ?? null, set: async (key, value) => void kept.set(key, value), del: async (key) => void kept.delete(key) };
};

/**
 * @param {object} c
 * @param {string} c.gatewayUrl   the gateway, e.g. https://gateway-staging.agentlater.io
 * @param {string} c.receiverUrl  public https base of the staging receiver, which the API delivers to
 * @param {string} [c.receiverToken] bearer token for reading what the receiver recorded; without it that check is skipped
 * @param {string} [c.receiverReadUrl] base for reading recorded deliveries (defaults to receiverUrl)
 * @param {typeof fetch} [c.fetch]
 * @param {(line: string) => void} [c.log]
 * @param {number} [c.pollMs] between reads of a task; 2 s keeps one run well under 60 requests a minute
 * @param {number} [c.timeoutMs] per wait
 * @returns {Promise<{ ok: boolean, report: string, rows: { name: string, ok: boolean, tasks: string[], seconds: number, seen: string }[] }>}
 */
export async function runA2aReport(c) {
  const log = c.log ?? (() => {});
  const pollMs = c.pollMs ?? 2_000;
  const timeoutMs = c.timeoutMs ?? 120_000;
  const started = Date.now();
  const run = `a2a-${started.toString(36)}-${randomBytes(4).toString("hex")}`;
  const hook = (name) => `${c.receiverUrl}/hook/${run}/${name}`;

  const rows = [];
  /** Runs one scenario. What it returns is what the report says was seen; what it throws is why it failed. */
  const scenario = async (name, body) => {
    const row = { name, ok: false, tasks: [], seconds: 0, seen: "" };
    rows.push(row);
    const from = Date.now();
    try {
      row.seen = await body(row.tasks);
      row.ok = true;
    } catch (e) {
      row.seen = e instanceof Error ? e.message : String(e);
    }
    row.seconds = (Date.now() - from) / 1000;
    log(`${row.ok ? "pass" : "FAIL"} ${name}: ${row.seen}`);
  };
  const identity = async () => {
    const session = await connect({ baseUrl: c.gatewayUrl, store: memory(), fetch: c.fetch, capabilities: [...CAPABILITIES, "redrive_task"] });
    return { ...session, project: await session.execute("create_project") };
  };
  const state = (task) => taskStateToJSON(task.status.state).replace("TASK_STATE_", "").toLowerCase();
  const cancel = (who, id) => who.a2a.cancelTask(CancelTaskRequest.fromJSON({ id }));
  /** Reads a task until it has ended. `seen` collects each delivery status on the way. */
  const settled = async (who, id, seen = []) => {
    for (const end = Date.now() + timeoutMs; ; ) {
      const task = await who.a2a.getTask({ id });
      const { status } = deliveryOf(task);
      if (seen.at(-1) !== status) seen.push(status);
      if (ended(task)) return task;
      check(Date.now() < end, `timed out waiting for ${id}, last ${status}`);
      await new Promise((resolve) => setTimeout(resolve, pollMs));
    }
  };
  /** Sends a delivery and notes its task in the scenario's row. */
  const send = async (who, tasks, payload, messageId) => {
    const task = await who.a2a.sendMessage(delivery(payload, messageId));
    if (!tasks.includes(task.id)) tasks.push(task.id);
    return task;
  };
  const statuses = (task) => deliveryOf(task).attempts.map((a) => a.http_status).join(", ");

  let first;
  let second;
  let completed;
  let failed;
  let waiting;
  await scenario("A new identity registers and makes its project", async () => {
    first = await identity();
    check(/^prj_/.test(first.project.id), "create_project returned no project");
    const empty = await first.a2a.listTasks(ListTasksRequest.fromJSON({}));
    check(empty.totalSize === 0 && empty.tasks.length === 0, `a new project lists ${empty.totalSize} tasks`);
    return `project ${first.project.id}, no tasks yet`;
  });
  check(first, `no identity, so nothing else can run: ${rows[0].seen}`);

  await scenario("Immediate success", async (tasks) => {
    const from = Date.now();
    const task = await send(first, tasks, { url: hook("ok"), body: JSON.stringify({ run }), headers: { "Content-Type": "application/json" }, capture: true });
    const answered = Date.now() - from;
    check(!ended(task), `the send returned a task already ${state(task)}`);
    check(task.contextId === task.id, "the task is not its own context");
    completed = await settled(first, task.id);
    const result = deliveryOf(completed);
    check(completed.status.state === TaskState.TASK_STATE_COMPLETED, `ended ${state(completed)}: ${statuses(completed)}`);
    check(result.attempt_count === 1 && result.attempts[0].http_status === 200, `attempts: ${statuses(completed)}`);
    check(result.response?.status === 200, "the captured response is not in the artifact");
    return `returned ${state(task)} in ${answered} ms, then completed after 1 attempt (HTTP 200)`;
  });

  await scenario("Delayed success", async (tasks) => {
    const task = await send(first, tasks, { url: hook("delayed"), delay: "5s" });
    const sent = deliveryOf(task);
    check(task.status.state === TaskState.TASK_STATE_SUBMITTED, `the send returned ${state(task)}`);
    check(sent.run_at_ms - sent.created_ms === 5_000, `delay 5s gave ${sent.run_at_ms - sent.created_ms} ms`);
    const done = await settled(first, task.id);
    const result = deliveryOf(done);
    check(done.status.state === TaskState.TASK_STATE_COMPLETED, `ended ${state(done)}: ${statuses(done)}`);
    check(result.finished_ms >= result.run_at_ms, "it was delivered before its time");
    return `returned submitted, completed ${((result.finished_ms - result.created_ms) / 1000).toFixed(1)} s after it was sent (delay 5s)`;
  });

  await scenario("Expected HTTP failure (404, not retried)", async (tasks) => {
    const task = await send(first, tasks, { url: hook("gone"), method: "DELETE" });
    failed = await settled(first, task.id);
    const result = deliveryOf(failed);
    check(failed.status.state === TaskState.TASK_STATE_FAILED, `ended ${state(failed)}: ${statuses(failed)}`);
    check(result.attempt_count === 1 && result.attempts[0].http_status === 404, `attempts: ${statuses(failed)}`);
    return "failed after 1 attempt (HTTP 404)";
  });

  await scenario("Retries exhausted (503 twice, max_attempts 2)", async (tasks) => {
    const task = await send(first, tasks, { url: hook("down"), max_attempts: 2 });
    const seen = [];
    const done = await settled(first, task.id, seen);
    check(done.status.state === TaskState.TASK_STATE_FAILED, `ended ${state(done)}: ${statuses(done)}`);
    check(statuses(done) === "503, 503", `attempts: ${statuses(done)}`);
    check(seen.includes("retrying"), `never read as retrying: ${seen.join(", ")}`);
    return `working while retrying, then failed after 2 attempts (HTTP ${statuses(done)})`;
  });

  await scenario("Cancellation", async (tasks) => {
    const task = await send(first, tasks, { url: hook("later"), delay: "1h" });
    const canceled = await cancel(first, task.id);
    check(canceled.status.state === TaskState.TASK_STATE_CANCELED, `the cancel returned ${state(canceled)}`);
    check((await cancel(first, task.id)).status.state === TaskState.TASK_STATE_CANCELED, "a repeated cancel did not return the canceled task");
    check((await first.a2a.getTask({ id: task.id })).status.state === TaskState.TASK_STATE_CANCELED, "the task does not read as canceled");
    check(completed, "needs the task of Immediate success");
    await refusal(cancel(first, completed.id), TaskNotCancelableError);
    return "canceled before its time, the same when asked again; a completed task is not cancelable (-32002)";
  });

  await scenario("Duplicate submission", async (tasks) => {
    const payload = { url: hook("dup"), delay: "1h" };
    const messageId = `${run}-dup`;
    waiting = await send(first, tasks, payload, messageId);
    const again = await send(first, tasks, payload, messageId);
    check(again.id === waiting.id, `the same message made a second task, ${again.id}`);
    await refusal(send(first, [], { ...payload, body: "changed" }, messageId), RequestMalformedError);
    return "the same message returned the same task; its messageId with another payload is refused (-32602)";
  });

  await scenario("ListTasks shows this run's tasks, latest change first", async () => {
    const made = rows.flatMap((row) => row.tasks);
    const page = await first.a2a.listTasks(ListTasksRequest.fromJSON({ includeArtifacts: true }));
    check(page.totalSize === made.length, `${made.length} tasks were made, ${page.totalSize} are listed`);
    check(
      page.tasks
        .map((t) => t.id)
        .sort()
        .join() === [...made].sort().join(),
      "the list is not the tasks this run made",
    );
    const times = page.tasks.map((t) => deliveryOf(t).updated_ms);
    check(times.join() === [...times].sort((a, b) => b - a).join(), "the list is not ordered by last change");
    return `${page.totalSize} tasks, as made`;
  });

  await scenario("A second host is refused", async (tasks) => {
    check(waiting, "needs the task of Duplicate submission");
    tasks.push(waiting.id);
    second = await identity();
    check(second.project.id !== first.project.id, "the second host got the first host's project");
    await refusal(second.a2a.getTask({ id: waiting.id }), TaskNotFoundError);
    await refusal(cancel(second, waiting.id), TaskNotFoundError);
    const theirs = await second.a2a.listTasks(ListTasksRequest.fromJSON({}));
    check(theirs.totalSize === 0 && theirs.tasks.length === 0, `the second host lists ${theirs.totalSize} tasks`);
    check((await first.a2a.getTask({ id: waiting.id })).status.state === TaskState.TASK_STATE_SUBMITTED, "the second host's cancel changed the task");
    return `project ${second.project.id}: GetTask and CancelTask answer task not found (-32001), ListTasks is empty, and the task is unchanged`;
  });

  await scenario("A failed task cannot be redriven", async (tasks) => {
    check(failed, "needs the task of Expected HTTP failure");
    tasks.push(failed.id);
    const error = await first.execute("redrive_task", { task_id: failed.id }).then(
      () => null,
      (e) => e,
    );
    check(error?.code === "a2a_task_not_redrivable", `redrive_task over Agent Auth: ${error?.code ?? "accepted"}`);
    check((await first.a2a.getTask({ id: failed.id })).status.state === TaskState.TASK_STATE_FAILED, "the failed task changed");
    return "redrive_task over Agent Auth is refused (a2a_task_not_redrivable); the task stays failed";
  });

  if (c.receiverToken) {
    await scenario("The destination saw each delivery the expected number of times", async () => {
      const res = await fetch(`${c.receiverReadUrl ?? c.receiverUrl}/runs/${run}`, { headers: { Authorization: `Bearer ${c.receiverToken}` } });
      check(res.ok, `receiver read returned ${res.status}`);
      const names = (await res.json()).deliveries.map((d) => d.name).sort();
      check(names.join() === "delayed,down,down,gone,ok", `the receiver recorded ${names.join() || "nothing"}`);
      return "ok 1, delayed 1, gone 1, down 2; the canceled and the duplicate task were never sent";
    });
  }

  await scenario("Clean up: cancel what is still scheduled, revoke both agents", async () => {
    if (waiting) check((await cancel(first, waiting.id)).status.state === TaskState.TASK_STATE_CANCELED, "the scheduled task was not canceled");
    // Nothing is asked of /a2a after this: the example client would register a new agent to answer it.
    for (const who of [first, second]) if (who) await who.agentAuth.disconnectAgent(who.agentId());
    return "nothing is left scheduled, and both agents are revoked";
  });

  const passed = rows.filter((row) => row.ok).length;
  const ok = passed === rows.length;
  const report = [
    "### A2A end-to-end report",
    "",
    `- Gateway: ${c.gatewayUrl}`,
    `- Run: \`${run}\`, ${new Date(started).toISOString()}`,
    `- Result: **${ok ? "PASS" : "FAIL"}**, ${passed} of ${rows.length} scenarios in ${((Date.now() - started) / 1000).toFixed(1)} s`,
    c.receiverToken ? "" : "- The receiver's record was not read (no RECEIVER_TOKEN); deliveries are judged by each task's attempts.\n",
    "| Scenario | Result | Seconds | Tasks | Seen |",
    "| --- | --- | --- | --- | --- |",
    ...rows.map((row) => `| ${row.name} | ${row.ok ? "pass" : "**FAIL**"} | ${row.seconds.toFixed(1)} | ${row.tasks.map((id) => `\`${id}\``).join(" ") || "-"} | ${row.seen.replaceAll("|", "\\|").replaceAll("\n", " ")} |`),
    "",
  ].join("\n");
  return { ok, report, rows };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [gatewayUrl = process.env.GATEWAY_URL, receiverUrl = process.env.RECEIVER_URL] = process.argv.slice(2);
  if (!gatewayUrl || !receiverUrl) {
    console.error("usage: node a2a-staging.mjs <gateway url> <receiver url>   (RECEIVER_TOKEN is optional)");
    process.exit(2);
  }
  try {
    const { ok, report } = await runA2aReport({
      gatewayUrl: gatewayUrl.replace(/\/$/, ""),
      receiverUrl: receiverUrl.replace(/\/$/, ""),
      receiverToken: process.env.RECEIVER_TOKEN,
      log: (line) => console.error(`a2a: ${line}`),
    });
    console.log(report);
    process.exit(ok ? 0 : 1);
  } catch (e) {
    console.error(`a2a run failed: ${e instanceof Error ? e.message : e}`);
    process.exit(1);
  }
}
