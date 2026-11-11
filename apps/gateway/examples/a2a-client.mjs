#!/usr/bin/env node
// An A2A client for AgentLater, from nothing to a delivered HTTP request, with no human login.
// It reads the Agent Card, registers with Agent Auth as the card says, makes or finds its project,
// schedules one delivery through A2A with the official SDK client, and polls it to its end. Run it
// again and it uses the same identity and finds the same project and task.
//
//   npm install @a2a-js/sdk@1.3.0 @auth/agent@0.6.2
//   node a2a-client.mjs https://gateway-staging.agentlater.io
//
// The gateway's URL is the first argument, or AGENTLATER_URL. `--new` schedules another delivery
// instead of reading the last one. AGENTLATER_DELIVER_URL is the URL to deliver to.
// AGENTLATER_WAIT_SECONDS is how long to wait for the task to end, 300 unless set.
//
// Exit code: 0 the request was delivered, 1 the task failed or was canceled, or the run itself
// failed, 2 no gateway URL was given, 3 the task had not ended when the wait ran out.
//
// State: one JSON file, ~/.agentlater/a2a-client-<gateway host>.json (AGENTLATER_STATE to put it
// elsewhere), written with mode 0600 in a directory made with mode 0700. It holds the host's and
// the agent's private keys and the ids of the agent, project and task; nothing else is written
// anywhere, and the keys are never sent: every request carries a JWT signed with them. Keep the
// file: the project belongs to the host key, and a new key gets a new project.

import { randomUUID } from "node:crypto";
import { mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { pathToFileURL } from "node:url";
import { SendMessageRequest, TaskState, taskStateToJSON } from "@a2a-js/sdk";
import { ClientFactory, DefaultAgentCardResolver, JsonRpcTransportFactory } from "@a2a-js/sdk/client";
import { TaskNotFoundError } from "@a2a-js/sdk/errors";
import { AgentAuthClient, KVStorage } from "@auth/agent";

/** The card's extension entry that says how to get a token. */
const AGENT_AUTH_EXTENSION = "https://agentlater.io/a2a/extensions/agent-auth/v1";

// a2a lets the agent call /a2a at all. Each operation then needs the capability it uses:
// SendMessage schedule_http (with run_at_ms or delay) or queue_http (with neither), GetTask
// get_task, ListTasks list_tasks, CancelTask cancel_task. create_project is run over Agent Auth.
export const CAPABILITIES = ["a2a", "create_project", "schedule_http", "queue_http", "get_task", "list_tasks", "cancel_task"];

/** What an agent that no longer exists is refused with: revoked, or past the 7 days an agent lives. */
const AGENT_GONE = ["agent_revoked", "absolute_lifetime_exceeded", "agent_not_found"];

const TERMINAL = [TaskState.TASK_STATE_COMPLETED, TaskState.TASK_STATE_FAILED, TaskState.TASK_STATE_CANCELED];
/** Whether a task has ended. The SDK client returns `status.state` as its numeric enum, not the JSON name. */
export const ended = (task) => TERMINAL.includes(task.status.state);
/** What a task's one artifact says of its delivery: status, attempts, times, and the response when captured. */
export const deliveryOf = (task) => task.artifacts[0]?.parts[0]?.content?.value;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** The state file as a key-value store: read once, rewritten whole on every change, readable by its owner only. */
async function fileStore(path) {
  let state = {};
  try {
    state = JSON.parse(await readFile(path, "utf8"));
  } catch (e) {
    if (e.code !== "ENOENT") throw e;
  }
  const save = async () => {
    await mkdir(dirname(path), { recursive: true, mode: 0o700 });
    await writeFile(`${path}.tmp`, JSON.stringify(state, null, 2), { mode: 0o600 });
    await rename(`${path}.tmp`, path);
  };
  return {
    get: async (key) => state[key] ?? null,
    set: async (key, value) => {
      state[key] = value;
      await save();
    },
    del: async (key) => {
      delete state[key];
      await save();
    },
  };
}

/**
 * An identity at the gateway and the two clients that use it: `a2a`, the official A2A client
 * with a token signed for every request, and `execute`, which runs a capability over Agent Auth.
 * `store` keeps the keys ({ get, set, del } over strings); `registered` says whether this call
 * had to register an agent.
 */
export async function connect({ baseUrl, store, fetch = globalThis.fetch, capabilities = CAPABILITIES, log = () => {} }) {
  // 1. The public Agent Card: where the JSON-RPC interface is, and how to get a token for it.
  const card = await new DefaultAgentCardResolver({ fetchImpl: fetch }).resolve(baseUrl).catch((e) => {
    throw new Error(`no Agent Card at ${baseUrl}/.well-known/agent-card.json (A2A is switched on per environment): ${e.message}`);
  });
  const auth = card.capabilities?.extensions?.find((e) => e.uri === AGENT_AUTH_EXTENSION)?.params;
  if (!auth) throw new Error("the Agent Card has no Agent Auth extension");

  // 2. Agent Auth. The SDK makes the host key the first time and keeps it in `store`; the same
  //    host key always reaches the same project.
  const storage = new KVStorage(store);
  const agentAuth = new AgentAuthClient({ storage, fetch, directoryUrl: null, allowDirectDiscovery: true, hostName: "AgentLater A2A example" });
  let agentId = await store.get("agentlater:agent");
  let registered = false;
  const register = async () => {
    const agent = await agentAuth.connectAgent({ provider: new URL(auth.discovery).origin, mode: "autonomous", capabilities, name: "a2a example" });
    if (agentId) await storage.deleteAgentConnection(agentId);
    agentId = agent.agentId;
    registered = true;
    await store.set("agentlater:agent", agentId);
    log(`registered agent ${agentId} of host ${agent.hostId}`);
  };
  if (!agentId) await register();

  // 3. A token for /a2a: aud is the interface's URL and capabilities is ["a2a"], as the card says.
  //    It is accepted once and lives 60 seconds, so every request signs its own.
  const token = async () => (await agentAuth.signJwt({ agentId, audience: auth.audience, capabilities: [auth.capability] })).token;

  // A refusal comes back as HTTP, before JSON-RPC. Two are worth a second try: 429 after the wait
  // it names, and an agent that is gone (they are revoked 7 days after they are made) once a new
  // one is registered under the same host key.
  const signedFetch = async (url, init) => {
    for (let tries = 1; ; tries++) {
      const response = await fetch(url, { ...init, headers: { ...Object.fromEntries(new Headers(init?.headers)), Authorization: `Bearer ${await token()}` } });
      if (response.ok || tries === 3) return response;
      const { error } = await response.clone().json().catch(() => ({}));
      if (response.status === 429) await sleep(1000 * Math.min(60, Number(response.headers.get("Retry-After")) || 5));
      else if (AGENT_GONE.includes(error)) await register();
      else return response;
    }
  };
  const a2a = await new ClientFactory({ transports: [new JsonRpcTransportFactory({ fetchImpl: signedFetch })] }).createFromAgentCard(card);

  const execute = async (capability, args = {}, retry = true) => {
    try {
      const result = await agentAuth.executeCapability({ agentId, capability, arguments: args });
      return result.data ?? result;
    } catch (e) {
      if (!retry || !AGENT_GONE.includes(e?.code)) throw e;
      await register();
      return execute(capability, args, false);
    }
  };

  return { card, a2a, execute, agentAuth, agentId: () => agentId, registered: () => registered };
}

/** A SendMessage request for one delivery. Built with fromJSON: the client drops the data of a part given as plain JSON. */
export const delivery = (payload, messageId = randomUUID()) =>
  SendMessageRequest.fromJSON({ message: { messageId, role: "ROLE_USER", parts: [{ data: payload, mediaType: "application/json" }] } });

const statePath = (baseUrl) => join(homedir(), ".agentlater", `a2a-client-${new URL(baseUrl).host.replace(/[^a-z0-9.-]/gi, "_")}.json`);

/**
 * One run: connect, make or find the project, schedule the delivery or find the one scheduled by
 * an earlier run, and poll it to its end, or for `waitMs` if it has not ended by then.
 */
export async function run({ baseUrl, state = statePath(baseUrl), payload = { url: "https://example.com/", method: "GET", delay: "5s" }, another = false, fetch, pollMs = 2000, waitMs = 300_000, log = console.log }) {
  const store = await fileStore(state);
  const { a2a, execute, agentId, registered } = await connect({ baseUrl, store, fetch, log });

  // A host has one project, and A2A needs it to exist: create_project makes it, or returns it.
  // It returns the project's signing secret every time, for the destination to check deliveries
  // with; this client has no use for it and neither keeps nor prints it.
  const project = await execute("create_project");
  log(`project ${project.id} ${(await store.get("agentlater:project")) === project.id ? "found again" : "ready"}`);
  await store.set("agentlater:project", project.id);

  if (another) for (const key of ["agentlater:task", "agentlater:message"]) await store.del(key);
  let task;
  const known = await store.get("agentlater:task");
  if (known) {
    task = await a2a.getTask({ id: known }).catch((e) => {
      // Tasks are deleted 30 days after they finish; after that they read as not found.
      if (e instanceof TaskNotFoundError) throw new Error(`task ${known} is no longer kept; run again with --new`);
      throw e;
    });
    log(`task ${task.id} found again`);
  } else {
    // The messageId is the idempotency key, kept before sending: if this stops before the task's
    // id is kept, the next run sends the same message and gets the same task, not a second one.
    const messageId = (await store.get("agentlater:message")) ?? randomUUID();
    await store.set("agentlater:message", messageId);
    // The answer is the task as it is now, never held until the delivery ends.
    task = await a2a.sendMessage(delivery(payload, messageId));
    await store.set("agentlater:task", task.id);
    log(`task ${task.id} scheduled`);
  }

  // Not for ever: a delivery that is retried can take an hour, and one scheduled for later waits
  // until then. The task goes on without this run, and the next run finds it again.
  const deadline = Date.now() + waitMs;
  while (!ended(task) && Date.now() < deadline) {
    await sleep(pollMs);
    task = await a2a.getTask({ id: task.id });
  }
  const result = deliveryOf(task);
  const now = taskStateToJSON(task.status.state);
  if (ended(task)) log(`task ${task.id} ${now}: ${result.attempt_count} attempt(s), last HTTP status ${result.attempts.at(-1)?.http_status ?? "none"}`);
  else log(`task ${task.id} is still ${now} after ${waitMs / 1000} s. It goes on without this run; run again to read it, with AGENTLATER_WAIT_SECONDS set to wait longer`);
  return { agentId: agentId(), registered: registered(), projectId: project.id, taskId: task.id, state: now, ended: ended(task), delivery: result };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const args = process.argv.slice(2);
  const baseUrl = (args.find((a) => !a.startsWith("--")) ?? process.env.AGENTLATER_URL)?.replace(/\/$/, "");
  if (!baseUrl) {
    console.error("usage: node a2a-client.mjs <gateway url> [--new]");
    process.exit(2);
  }
  const url = process.env.AGENTLATER_DELIVER_URL;
  const wait = Number(process.env.AGENTLATER_WAIT_SECONDS);
  try {
    const task = await run({ baseUrl, state: process.env.AGENTLATER_STATE, another: args.includes("--new"), payload: url ? { url, delay: "5s" } : undefined, waitMs: wait > 0 ? wait * 1000 : undefined });
    process.exit(task.state === "TASK_STATE_COMPLETED" ? 0 : task.ended ? 1 : 3);
  } catch (e) {
    console.error(`failed: ${e instanceof Error ? e.message : e}`);
    process.exit(1);
  }
}
