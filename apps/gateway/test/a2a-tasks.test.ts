// SendMessage and GetTask at /a2a, as an adapter over the capabilities: which capability each
// runs and with what, and what comes back. The capabilities themselves are stood in for here; the
// e2e suite runs the same operations against the real API.

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { capabilities } from "@runlater/contracts/capabilities";
import { describe, expect, it } from "vitest";
import { Caller, handleA2a } from "../src/a2a";
import { ApiError } from "../src/api";
import { ArgumentError } from "../src/capabilities";
import worker, { type Env } from "../src/index";

type Json = Record<string, any>;
const ORIGIN = "https://gateway.test";
const SCHEMA_URL = `${ORIGIN}/a2a/schemas/deliver-http/v1.json`;
const on = { PUBLIC_URL: ORIGIN, A2A_ENABLED: "true" } as unknown as Env;
const ctx = { waitUntil() {}, passThroughOnException() {} } as unknown as ExecutionContext;
const TASK_ID = "task_0123456789abcdef0123";

/** A task as get_task returns it: here one whose request carried credentials in every place it can. */
const view = (over: Json = {}): Json => ({
  id: TASK_ID,
  status: "scheduled",
  method: "POST",
  url: "https://dest.example.com/hook/path-token?key=query-token",
  header_names: ["Authorization"],
  body_bytes: 12,
  run_at_ms: 1_800_000_000_000,
  next_attempt_ms: 1_800_000_000_000,
  max_attempts: 5,
  attempt_count: 0,
  idempotency_key: "key-token",
  callback_url: null,
  callback_task_id: null,
  callback_for: null,
  inbox_seq: null,
  capture: false,
  response: null,
  wake_call: false,
  failure: null,
  report_failures_only: false,
  lane: null,
  cron: null,
  alert_for: null,
  watch: null,
  triggered_by: null,
  created_ms: 1_799_999_000_000,
  updated_ms: 1_799_999_000_500,
  finished_ms: null,
  attempts: [],
  ...over,
});

const ALL = ["a2a", "schedule_http", "queue_http", "get_task", "list_tasks", "cancel_task"];
/** /a2a for a verified caller with these grants; `answer` stands in for running a capability. */
function adapter(grants = ALL, answer: (capability: string, args: Json) => unknown = () => view()) {
  const ran: [string, Json][] = [];
  const send = async (method: string, params: unknown) => {
    const request = new Request(`${ORIGIN}/a2a`, {
      method: "POST",
      headers: { "A2A-Version": "1.0", Authorization: "Bearer verified" },
      body: JSON.stringify({ jsonrpc: "2.0", id: 7, method, params }),
    });
    const res = await handleA2a(
      request,
      on,
      async () => new Caller("host_1", new Set(grants)),
      async (hostId, capability, args) => {
        expect(hostId).toBe("host_1");
        ran.push([capability, args]);
        return answer(capability, args);
      },
    );
    return { status: res.status, body: (await res.json()) as Json };
  };
  return { ran, send };
}
const message = (data: unknown, over: Json = {}) => ({ message: { messageId: "m-1", role: "ROLE_USER", parts: [{ data, mediaType: "application/json" }], ...over } });
const refuses = (status: number, code: string, message: string, field?: string) => () => {
  throw new ApiError(status, { error: { code, message, ...(field ? { field } : {}) } });
};
const REQUEST = { url: "https://dest.example.com/hook", method: "POST", headers: { "X-Order": "42" }, body: '{"order":42}', max_attempts: 3, capture: true };

describe("SendMessage", () => {
  it("with a delay or a time runs schedule_http with the payload as it is and returns the task, submitted", async () => {
    for (const when of [{ delay: "15m" }, { run_at_ms: 1_800_000_000_000 }]) {
      const { ran, send } = adapter();
      const { body } = await send("SendMessage", message({ ...REQUEST, ...when, idempotency_key: "order-42" }));
      expect(ran).toEqual([["schedule_http", { ...REQUEST, ...when, idempotency_key: "order-42" }]]);
      expect(body).toMatchObject({ jsonrpc: "2.0", id: 7, result: { task: { id: TASK_ID, status: { state: "TASK_STATE_SUBMITTED" } } } });
      expect(body.result.message).toBeUndefined();
    }
  });

  it("with neither runs queue_http", async () => {
    const { ran, send } = adapter();
    await send("SendMessage", message({ url: "https://dest.example.com/hook", idempotency_key: "k" }));
    await send("SendMessage", message({ url: "https://dest.example.com/hook", idempotency_key: "k", delay: null, run_at_ms: null }));
    expect(ran.map(([capability]) => capability)).toEqual(["queue_http", "queue_http"]);
  });

  it("returns the task in its current state without waiting, whether or not returnImmediately is set", async () => {
    for (const configuration of [undefined, {}, { returnImmediately: false }, { returnImmediately: true }]) {
      const { ran, send } = adapter(ALL, () => view({ status: "delivering" }));
      const { body } = await send("SendMessage", { ...message({ url: "https://dest.example.com/hook" }), configuration });
      expect(body.result.task.status.state).toBe("TASK_STATE_WORKING");
      // One call, to schedule: the task is never read again to see whether it has finished.
      expect(ran).toHaveLength(1);
    }
  });

  it("uses the payload's idempotency_key, else the message's messageId", async () => {
    const { ran, send } = adapter();
    await send("SendMessage", message({ url: "https://dest.example.com/hook", idempotency_key: "from-payload" }));
    await send("SendMessage", message({ url: "https://dest.example.com/hook" }, { messageId: "0b5f6c1e-6a0e-4a53-9f43-7a8f1e2d3c4b" }));
    expect(ran.map(([, args]) => args.idempotency_key)).toEqual(["from-payload", "0b5f6c1e-6a0e-4a53-9f43-7a8f1e2d3c4b"]);
  });

  it("uses a digest of a messageId that could not be an idempotency key: the same id gives the same key", async () => {
    const pattern = new RegExp(capabilities.find((c) => c.name === "schedule_http")!.input.properties.idempotency_key!.pattern as string);
    const { ran, send } = adapter();
    const odd = ["urn:uuid:0b5f6c1e", "msg 1", "x".repeat(129), "urn:uuid:0b5f6c1e", "ünï"];
    for (const messageId of odd) await send("SendMessage", message({ url: "https://dest.example.com/hook" }, { messageId }));
    const keys = ran.map(([, args]) => args.idempotency_key as string);
    for (const key of keys) expect(key).toMatch(pattern);
    expect(keys[3]).toBe(keys[0]);
    expect(new Set(keys).size).toBe(4);
    expect(odd).not.toContain(keys[0]);
  });

  it("refuses a message without a messageId, which would have no key", async () => {
    const { ran, send } = adapter();
    for (const messageId of [undefined, ""]) {
      const { body } = await send("SendMessage", message({ url: "https://dest.example.com/hook" }, { messageId }));
      expect(body.error).toMatchObject({ code: -32602, data: [{ metadata: { field: "message.messageId" } }] });
    }
    expect(ran).toEqual([]);
  });

  it("answers the same key with a different payload with -32602 and says which key", async () => {
    const conflict = refuses(409, "idempotency_conflict", "idempotency_key was already used for a different request");
    const given = await adapter(ALL, conflict).send("SendMessage", message({ url: "https://dest.example.com/other", idempotency_key: "order-42" }));
    expect(given.body.error.code).toBe(-32602);
    expect(given.body.error.message).toContain("already used for a different request");
    expect(given.body.error.data).toEqual([
      { "@type": "type.googleapis.com/google.rpc.ErrorInfo", reason: "INVALID_PARAMS", domain: "a2a-protocol.org", metadata: { field: "message.parts[0].data.idempotency_key", code: "idempotency_conflict" } },
    ]);
    const fromMessage = await adapter(ALL, conflict).send("SendMessage", message({ url: "https://dest.example.com/other" }));
    expect(fromMessage.body.error.code).toBe(-32602);
    expect(fromMessage.body.error.data[0].metadata).toEqual({ field: "message.messageId", code: "idempotency_conflict" });
  });

  it("schedules nothing for a text-only message, and says what to send", async () => {
    const { ran, send } = adapter();
    const text = { message: { messageId: "m-1", role: "ROLE_USER", parts: [{ text: "POST to https://dest.example.com/hook in 15 minutes" }] } };
    const { body } = await send("SendMessage", text);
    expect(body.result).toBeUndefined();
    expect(body.error.code).toBe(-32005);
    expect(body.error.message).toContain(SCHEMA_URL);
    expect(body.error.message).toContain("data part");
    // Any part that is not JSON data is refused the same way, alone or beside a data part.
    const others = [
      [{ text: "now" }, { data: REQUEST, mediaType: "application/json" }],
      [{ url: "https://files.example.com/request.json", mediaType: "application/json" }],
      [{ raw: "e30=", mediaType: "application/json" }],
      [{ data: REQUEST, mediaType: "text/plain" }],
    ];
    for (const parts of others) expect((await send("SendMessage", message(null, { parts }))).body.error.code, JSON.stringify(parts)).toBe(-32005);
    expect(ran).toEqual([]);
  });

  it("schedules nothing for a message that is not exactly one JSON object, naming parts", async () => {
    const { ran, send } = adapter();
    const data = (value: unknown) => ({ data: value, mediaType: "application/json" });
    for (const parts of [[], [data(REQUEST), data(REQUEST)], [data("https://dest.example.com/hook")], [data([REQUEST])], [data(15)], [{}]]) {
      const { body } = await send("SendMessage", message(null, { parts }));
      expect(body.error, JSON.stringify(parts)).toMatchObject({ code: -32602, data: [{ metadata: { field: "message.parts" } }] });
      expect(body.error.message).toContain(SCHEMA_URL);
    }
    expect(ran).toEqual([]);
  });

  it("takes a data part that names no media type", async () => {
    const { ran, send } = adapter();
    await send("SendMessage", message(null, { parts: [{ data: { url: "https://dest.example.com/hook" } }] }));
    expect(ran).toHaveLength(1);
  });

  it("refuses a field the payload does not have, by name, before anything runs", async () => {
    const { ran, send } = adapter();
    for (const field of ["callback_url", "lane", "queue", "via", "task_id"]) {
      const { body } = await send("SendMessage", message({ url: "https://dest.example.com/hook", [field]: "inbox" }));
      expect(body.error, field).toMatchObject({ code: -32602, data: [{ metadata: { field: `message.parts[0].data.${field}` } }] });
      expect(body.error.message).toContain(field);
    }
    expect(ran).toEqual([]);
  });

  it("translates the API's validation errors to -32602, naming the field", async () => {
    const api = await adapter(ALL, refuses(400, "invalid_request", "url must be an https:// URL", "url")).send("SendMessage", message({ url: "http://dest.example.com/" }));
    expect(api.body.error).toMatchObject({ code: -32602, message: "url must be an https:// URL", data: [{ reason: "INVALID_PARAMS", metadata: { field: "message.parts[0].data.url", code: "invalid_request" } }] });
    // What the gateway refuses about arguments before the API is called.
    const argument = () => {
      throw new ArgumentError("queue_http sends as soon as possible", "delay");
    };
    const gateway = await adapter(ALL, argument).send("SendMessage", message({ url: "https://dest.example.com/" }));
    expect(gateway.body.error).toMatchObject({ code: -32602, data: [{ metadata: { field: "message.parts[0].data.delay" } }] });
  });

  it("answers what the API refuses for another reason with -32000 and the API's code and message", async () => {
    const cases: [number, string][] = [[402, "budget_exhausted"], [403, "trial_expired"], [404, "project_not_found"], [429, "task_limit"], [429, "rate_limited"]];
    for (const [status, code] of cases) {
      const { body } = await adapter(ALL, refuses(status, code, `why ${code}`)).send("SendMessage", message({ url: "https://dest.example.com/" }));
      expect(body.error, code).toEqual({ code: -32000, message: `${code}: why ${code}` });
    }
  });

  it("answers a failure of AgentLater's own with an internal error that says nothing about it", async () => {
    const broken = [
      refuses(500, "internal_error", "no such column: via"),
      refuses(503, "api_unavailable", "API returned 503"),
      // The API turning down the gateway's own assertion, as an API one version behind would.
      refuses(401, "unauthorized", "invalid assertion"),
      () => {
        throw new TypeError("secret detail");
      },
      () => view({ status: "paused" }),
    ];
    for (const answer of broken) {
      const { body } = await adapter(ALL, answer).send("SendMessage", message({ url: "https://dest.example.com/" }));
      expect(body.error).toEqual({ code: -32603, message: "AgentLater could not complete this request; try again" });
    }
  });

  it("does not take a further message for a task: the caller's own is unsupported, any other is not found", async () => {
    const own = adapter();
    const further = await own.send("SendMessage", message({ url: "https://dest.example.com/hook" }, { taskId: TASK_ID }));
    expect(further.body.error.code).toBe(-32004);
    expect(own.ran).toEqual([["get_task", { task_id: TASK_ID }]]);

    for (const answer of [refuses(404, "task_not_found", "no such task in this project"), refuses(404, "project_not_found", "call create_project first")]) {
      const other = adapter(ALL, answer);
      const { body } = await other.send("SendMessage", message({ url: "https://dest.example.com/hook" }, { taskId: "task_ffffffffffffffffffff", contextId: "task_ffffffffffffffffffff" }));
      expect(body.error).toMatchObject({ code: -32001, message: "Task not found" });
      expect(other.ran.map(([capability]) => capability)).toEqual(["get_task"]);
    }
  });

  it("refuses a context id the caller supplies, and generates none for the answer", async () => {
    const { ran, send } = adapter();
    for (const contextId of [TASK_ID, "ctx-of-another-host", "0b5f6c1e-6a0e-4a53-9f43-7a8f1e2d3c4b"]) {
      const { body } = await send("SendMessage", message({ url: "https://dest.example.com/hook" }, { contextId }));
      expect(body.error, contextId).toMatchObject({ code: -32602, data: [{ metadata: { field: "message.contextId" } }] });
      expect(JSON.stringify(body)).not.toContain('"contextId"');
    }
    expect(ran).toEqual([]);
  });

  it("refuses a push notification config sent with the message", async () => {
    const { ran, send } = adapter();
    const configuration = { taskPushNotificationConfig: { url: "https://client.example.com/push" } };
    expect((await send("SendMessage", { ...message({ url: "https://dest.example.com/hook" }), configuration })).body.error.code).toBe(-32003);
    expect(ran).toEqual([]);
  });
});

/** Decision 4 of #209. A status the ledger gains must be decided and added here and to the adapter's table. */
const DECIDED: Record<string, string> = {
  scheduled: "TASK_STATE_SUBMITTED",
  delivering: "TASK_STATE_WORKING",
  retrying: "TASK_STATE_WORKING",
  succeeded: "TASK_STATE_COMPLETED",
  failed: "TASK_STATE_FAILED",
  canceled: "TASK_STATE_CANCELED",
};
/** The ledger's statuses, from the enum that defines them, as they are written on the wire. */
function ledgerStatuses(): string[] {
  const source = readFileSync(join(dirname(fileURLToPath(import.meta.url)), "../../../crates/core/src/task.rs"), "utf8");
  const variants = /pub enum Status \{([^}]*)\}/.exec(source)![1]!;
  return [...variants.matchAll(/\b([A-Z][A-Za-z]*)\b/g)].map((m) => m[1]!.replace(/(?<!^)[A-Z]/g, (c) => `_${c}`).toLowerCase());
}

describe("GetTask", () => {
  const get = async (task: Json, params: Json = {}) => (await adapter(ALL, () => task).send("GetTask", { id: TASK_ID, ...params })).body;

  it("maps every delivery status the ledger has to the decided A2A state", async () => {
    const statuses = ledgerStatuses();
    expect(statuses).toContain("scheduled");
    expect(statuses.sort()).toEqual(Object.keys(DECIDED).sort());
    for (const status of statuses) {
      const body = await get(view({ status }));
      expect(body.result?.status.state, status).toBe(DECIDED[status]);
    }
  });

  it("never reports scheduled, delivering or retrying work as completed", async () => {
    for (const status of ["scheduled", "delivering", "retrying"]) {
      const { state } = (await get(view({ status }))).result.status;
      expect(["TASK_STATE_SUBMITTED", "TASK_STATE_WORKING"], status).toContain(state);
    }
  });

  it("runs get_task with the id and returns the task itself, whose id and context are the delivery task's id", async () => {
    const { ran, send } = adapter();
    const { body } = await send("GetTask", { id: TASK_ID, historyLength: 10 });
    expect(ran).toEqual([["get_task", { task_id: TASK_ID }]]);
    expect(body.result).toMatchObject({ id: TASK_ID, contextId: TASK_ID, status: { state: "TASK_STATE_SUBMITTED", timestamp: "2027-01-15T07:43:20.500Z" } });
    // No message is kept, so there is no history to return, whatever length is asked for.
    expect(body.result.history).toBeUndefined();
  });

  it("carries the delivery status, timestamps and attempts in one JSON artifact", async () => {
    const attempts = [
      { number: 1, started_ms: 1_800_000_000_100, finished_ms: 1_800_000_000_300, outcome: "http_503", http_status: 503 },
      { number: 2, started_ms: 1_800_000_030_000, finished_ms: 1_800_000_030_200, outcome: "http_200", http_status: 200 },
    ];
    const done = view({ status: "succeeded", next_attempt_ms: null, attempt_count: 2, updated_ms: 1_800_000_030_200, finished_ms: 1_800_000_030_200, attempts });
    const { artifacts } = (await get(done)).result;
    expect(artifacts).toHaveLength(1);
    expect(artifacts[0]).toMatchObject({ artifactId: "delivery" });
    expect(artifacts[0].parts).toEqual([
      {
        mediaType: "application/json",
        data: {
          task_id: TASK_ID,
          status: "succeeded",
          run_at_ms: 1_800_000_000_000,
          next_attempt_ms: null,
          max_attempts: 5,
          attempt_count: 2,
          failure: null,
          created_ms: 1_799_999_000_000,
          updated_ms: 1_800_000_030_200,
          finished_ms: 1_800_000_030_200,
          attempts,
        },
      },
    ]);
    // A 2xx is the destination's answer to a delivery, and the artifact says no more than that.
    expect(artifacts[0].description).toContain("2xx");
  });

  it("carries the captured response only when capture was asked", async () => {
    const response = { attempt: 1, status: 200, headers: { "content-type": "application/json" }, body: '{"ok":true}', body_bytes: 11, truncated: false };
    const captured = (await get(view({ status: "succeeded", capture: true, response }))).result.artifacts[0].parts[0].data;
    expect(captured.response).toEqual(response);
    for (const task of [view({ status: "succeeded" }), view({ capture: true, response: null }), view({ capture: false, response })]) {
      expect((await get(task)).result.artifacts[0].parts[0].data).not.toHaveProperty("response");
    }
  });

  it("leaves out everything of the request that can hold a credential", async () => {
    const text = JSON.stringify(await get(view({ callback_url: "https://cb.example.com/callback-token", lane: "lane-token" })));
    for (const secret of ["path-token", "query-token", "key-token", "callback-token", "lane-token", "Authorization", "dest.example.com"]) expect(text).not.toContain(secret);
  });

  it("answers TaskNotFoundError the same for a task that is not there, a caller with no project and an id that could be no task's", async () => {
    const answers = [
      refuses(404, "task_not_found", "no such task in this project"),
      refuses(404, "project_not_found", "call create_project before using other capabilities"),
      () => {
        throw new ArgumentError("task_id is not a valid task id", "task_id");
      },
    ];
    const bodies = [];
    for (const answer of answers) bodies.push((await adapter(ALL, answer).send("GetTask", { id: TASK_ID })).body);
    expect(bodies[0]).toEqual({
      jsonrpc: "2.0",
      id: 7,
      error: { code: -32001, message: "Task not found", data: [{ "@type": "type.googleapis.com/google.rpc.ErrorInfo", reason: "TASK_NOT_FOUND", domain: "a2a-protocol.org" }] },
    });
    expect(bodies[1]).toEqual(bodies[0]);
    expect(bodies[2]).toEqual(bodies[0]);
  });

  it("does not call a task missing when the read was refused for another reason", async () => {
    const limited = await adapter(ALL, refuses(429, "rate_limited", "more than 300 calls in a minute")).send("GetTask", { id: TASK_ID });
    expect(limited.body.error).toEqual({ code: -32000, message: "rate_limited: more than 300 calls in a minute" });
    const broken = await adapter(ALL, refuses(503, "api_unavailable", "API returned 503")).send("GetTask", { id: TASK_ID });
    expect(broken.body.error.code).toBe(-32603);
  });
});

describe("CancelTask", () => {
  const cancel = (answer: (capability: string, args: Json) => unknown) => adapter(ALL, answer).send("CancelTask", { id: TASK_ID });

  it("runs cancel_task with the id and returns a task that had not started as canceled", async () => {
    const { ran, send } = adapter(ALL, () => view({ status: "canceled", finished_ms: 1_799_999_000_500 }));
    const { body } = await send("CancelTask", { id: TASK_ID, metadata: { why: "changed my mind" } });
    expect(ran).toEqual([["cancel_task", { task_id: TASK_ID }]]);
    expect(body.result).toMatchObject({ id: TASK_ID, contextId: TASK_ID, status: { state: "TASK_STATE_CANCELED", timestamp: "2027-01-15T07:43:20.500Z" } });
    expect(body.result.status.message).toBeUndefined();
    expect(body.result.artifacts[0].parts[0].data).toMatchObject({ task_id: TASK_ID, status: "canceled" });
  });

  it("answers TaskNotCancelableError for a task that has finished, and says how it finished", async () => {
    for (const how of ["succeeded", "failed"]) {
      const { body } = await cancel(refuses(409, "task_finished", `task already ${how}`));
      expect(body).toEqual({
        jsonrpc: "2.0",
        id: 7,
        error: {
          code: -32002,
          message: `task already ${how}`,
          data: [{ "@type": "type.googleapis.com/google.rpc.ErrorInfo", reason: "TASK_NOT_CANCELABLE", domain: "a2a-protocol.org" }],
        },
      });
    }
  });

  it("returns a task in flight as working, and says the cancel waits on the attempt: nothing claims it was canceled", async () => {
    const { body } = await cancel(() => view({ status: "delivering", attempt_count: 1 }));
    expect(body.result.status.state).toBe("TASK_STATE_WORKING");
    expect(body.result.artifacts[0].parts[0].data.status).toBe("delivering");
    const { message } = body.result.status;
    expect(message).toMatchObject({ role: "ROLE_AGENT", taskId: TASK_ID, contextId: TASK_ID });
    expect(message.messageId).toBeTruthy();
    expect(message.parts).toEqual([{ text: "An attempt is being sent and cannot be stopped. If it succeeds the task completes; if not, the task is canceled and is not retried." }]);
    expect(JSON.stringify(body)).not.toContain("TASK_STATE_CANCELED");
  });

  it("returns a task that was already canceled as canceled, however often it is asked", async () => {
    const { ran, send } = adapter(ALL, () => view({ status: "canceled" }));
    for (let i = 0; i < 2; i++) expect((await send("CancelTask", { id: TASK_ID })).body.result.status.state).toBe("TASK_STATE_CANCELED");
    expect(ran).toHaveLength(2);
  });

  it("answers TaskNotFoundError the same for a task that is not there, a caller with no project and an id that could be no task's", async () => {
    const answers = [
      refuses(404, "task_not_found", "no such task in this project"),
      refuses(404, "project_not_found", "call create_project before using other capabilities"),
      () => {
        throw new ArgumentError("task_id is not a valid task id", "task_id");
      },
    ];
    const bodies = [];
    for (const answer of answers) bodies.push((await cancel(answer)).body);
    expect(bodies[0]).toMatchObject({ error: { code: -32001, message: "Task not found" } });
    expect(bodies[1]).toEqual(bodies[0]);
    expect(bodies[2]).toEqual(bodies[0]);
  });
});

describe("ListTasks", () => {
  /** A task as list_tasks returns it: without its attempts. */
  const listed = (over: Json = {}) => {
    const { attempts: _, ...task } = view(over);
    return task;
  };
  const OTHER_ID = "task_ffffffffffffffffffff";
  const page = (over: Json = {}) => () => ({ tasks: [listed({ status: "succeeded" }), listed({ id: OTHER_ID, updated_ms: 1_799_999_000_000 })], next_cursor: null, total: 2, ...over });
  const list = async (params: Json, answer = page()) => {
    const { ran, send } = adapter(ALL, answer);
    return { ...(await send("ListTasks", params)), ran };
  };
  const refusedFor = (field: string) => ({ code: -32602, data: [{ reason: "INVALID_PARAMS", metadata: { field } }] });

  it("runs list_tasks for a page of 50 and returns its tasks in the order given, with the page size and the total", async () => {
    const { ran, body } = await list({});
    expect(ran).toEqual([["list_tasks", { limit: 50 }]]);
    expect(body).toMatchObject({
      jsonrpc: "2.0",
      id: 7,
      result: {
        tasks: [
          { id: TASK_ID, contextId: TASK_ID, status: { state: "TASK_STATE_COMPLETED", timestamp: "2027-01-15T07:43:20.500Z" } },
          { id: OTHER_ID, contextId: OTHER_ID, status: { state: "TASK_STATE_SUBMITTED", timestamp: "2027-01-15T07:43:20.000Z" } },
        ],
        pageSize: 50,
        totalSize: 2,
      },
    });
  });

  it("gives the next page's token, and an empty one on the last page: all four fields are always there", async () => {
    const more = await list({ pageSize: 2 }, page({ next_cursor: "1799999000000.41", total: 7 }));
    expect(more.ran).toEqual([["list_tasks", { limit: 2 }]]);
    expect(more.body.result).toMatchObject({ nextPageToken: "1799999000000.41", pageSize: 2, totalSize: 7 });

    const next = await list({ pageSize: 2, pageToken: "1799999000000.41" });
    expect(next.ran).toEqual([["list_tasks", { limit: 2, cursor: "1799999000000.41" }]]);
    expect(next.body.result.nextPageToken).toBe("");

    const none = await list({}, page({ tasks: [], total: 0 }));
    expect(none.body.result).toEqual({ tasks: [], nextPageToken: "", pageSize: 50, totalSize: 0 });
  });

  it("refuses a page size outside 1 to 100 and a token it did not give, by name, before anything runs", async () => {
    for (const pageSize of [0, 101, -1, 2.5]) {
      const { ran, body } = await list({ pageSize });
      expect(body.error, String(pageSize)).toMatchObject(refusedFor("pageSize"));
      expect(ran).toEqual([]);
    }
    for (const pageToken of ["x", "1.2.3", "1&status=failed", "42"]) {
      const { ran, body } = await list({ pageToken });
      expect(body.error, pageToken).toMatchObject(refusedFor("pageToken"));
      expect(ran).toEqual([]);
    }
    expect((await list({ pageSize: 1 })).ran).toEqual([["list_tasks", { limit: 1 }]]);
    expect((await list({ pageSize: 100 })).ran).toEqual([["list_tasks", { limit: 100 }]]);
  });

  it("filters by state through the decided table: working is both delivering and retrying", async () => {
    // Written out, not derived from the table the code uses.
    const statusesOf: Json = {
      TASK_STATE_SUBMITTED: "scheduled",
      TASK_STATE_WORKING: "delivering,retrying",
      TASK_STATE_COMPLETED: "succeeded",
      TASK_STATE_FAILED: "failed",
      TASK_STATE_CANCELED: "canceled",
    };
    for (const [state, status] of Object.entries(statusesOf)) {
      expect((await list({ status: state })).ran, state).toEqual([["list_tasks", { limit: 50, status }]]);
    }
    // Every delivery status is reached by exactly one state's filter.
    expect(Object.values(statusesOf).join(",").split(",").sort()).toEqual(ledgerStatuses().sort());
    for (const none of [undefined, "TASK_STATE_UNSPECIFIED"]) expect((await list({ status: none })).ran).toEqual([["list_tasks", { limit: 50 }]]);
  });

  it("lists nothing for a state no delivery is ever in, and refuses one the protocol does not have", async () => {
    for (const status of ["TASK_STATE_INPUT_REQUIRED", "TASK_STATE_AUTH_REQUIRED", "TASK_STATE_REJECTED"]) {
      const { ran, body } = await list({ status, pageSize: 10 });
      expect(body.result, status).toEqual({ tasks: [], nextPageToken: "", pageSize: 10, totalSize: 0 });
      expect(ran).toEqual([]);
    }
    const { ran, body } = await list({ status: "TASK_STATE_DONE" });
    expect(body.error).toMatchObject(refusedFor("status"));
    expect(ran).toEqual([]);
  });

  it("lists from a status timestamp on, and refuses one that is not a time", async () => {
    const { ran } = await list({ statusTimestampAfter: "2027-01-15T07:43:20.500Z", status: "TASK_STATE_FAILED" });
    expect(ran).toEqual([["list_tasks", { limit: 50, status: "failed", updated_since_ms: 1_799_999_000_500 }]]);
    // With an offset, and from before any task could be: the same instant, and everything.
    expect((await list({ statusTimestampAfter: "2027-01-15T09:43:20+02:00" })).ran).toEqual([["list_tasks", { limit: 50, updated_since_ms: 1_799_999_000_000 }]]);
    expect((await list({ statusTimestampAfter: "1969-12-31T23:59:59Z" })).ran).toEqual([["list_tasks", { limit: 50, updated_since_ms: 0 }]]);
    for (const statusTimestampAfter of ["yesterday", "-1", "1799999000500", "2027-13-45T07:43:20Z", "2027-01-15T07:43:20"]) {
      const refused = await list({ statusTimestampAfter });
      expect(refused.body.error, statusTimestampAfter).toMatchObject(refusedFor("statusTimestampAfter"));
      expect(refused.ran).toEqual([]);
    }
  });

  it("refuses a context id, by name, rather than ignore it: a task is its own context", async () => {
    const { ran, body } = await list({ contextId: TASK_ID });
    expect(body.error).toMatchObject(refusedFor("contextId"));
    expect(body.error.message).toMatch(/GetTask/);
    expect(ran).toEqual([]);
  });

  it("leaves artifacts out unless they are asked for, and then carries what a list knows: no attempts and no response", async () => {
    for (const includeArtifacts of [undefined, false]) {
      const { body } = await list({ includeArtifacts });
      for (const task of body.result.tasks) expect("artifacts" in task).toBe(false);
    }
    const { body } = await list({ includeArtifacts: true }, page({ tasks: [listed({ status: "succeeded", capture: true, attempt_count: 1, finished_ms: 1_799_999_000_500 })] }));
    const [artifact] = body.result.tasks[0].artifacts;
    expect(artifact).toMatchObject({ artifactId: "delivery", parts: [{ mediaType: "application/json" }] });
    expect(artifact.parts[0].data).toEqual({
      task_id: TASK_ID,
      status: "succeeded",
      run_at_ms: 1_800_000_000_000,
      next_attempt_ms: 1_800_000_000_000,
      max_attempts: 5,
      attempt_count: 1,
      failure: null,
      created_ms: 1_799_999_000_000,
      updated_ms: 1_799_999_000_500,
      finished_ms: 1_799_999_000_500,
    });
    expect(JSON.stringify(body)).not.toMatch(/query-token|path-token|key-token|Authorization/);
  });

  it("returns no history, whatever length is asked for, as GetTask does", async () => {
    for (const historyLength of [undefined, 0, 10]) {
      const { ran, body } = await list({ historyLength });
      expect(ran).toEqual([["list_tasks", { limit: 50 }]]);
      for (const task of body.result.tasks) expect(task.history).toBeUndefined();
    }
  });

  it("tells a caller with no project so, as SendMessage does, and hides a failure of AgentLater's own", async () => {
    const none = await list({}, refuses(404, "project_not_found", "call create_project before using other capabilities"));
    expect(none.body.error).toEqual({ code: -32000, message: "project_not_found: call create_project before using other capabilities" });
    const broken = await list({}, refuses(503, "api_unavailable", "API returned 503"));
    expect(broken.body.error).toMatchObject({ code: -32603, message: "AgentLater could not complete this request; try again" });
  });
});

describe("an agent without the grant for the capability an operation uses", () => {
  const cases: [string, string, unknown][] = [
    ["schedule_http", "SendMessage", message({ url: "https://dest.example.com/hook", delay: "15m" })],
    ["queue_http", "SendMessage", message({ url: "https://dest.example.com/hook" })],
    ["get_task", "GetTask", { id: TASK_ID }],
    ["get_task", "SendMessage", message({ url: "https://dest.example.com/hook" }, { taskId: TASK_ID })],
    ["cancel_task", "CancelTask", { id: TASK_ID }],
    ["list_tasks", "ListTasks", {}],
    // A state no delivery is ever in lists nothing, but not for an agent that may not list.
    ["list_tasks", "ListTasks", { status: "TASK_STATE_REJECTED" }],
  ];

  it("is refused with HTTP 403 that names the capability, and nothing runs", async () => {
    for (const [capability, method, params] of cases) {
      const { ran, send } = adapter(ALL.filter((c) => c !== capability));
      const { status, body } = await send(method, params);
      expect(status, capability).toBe(403);
      expect(body).toEqual({ error: "capability_not_granted", message: `this agent does not hold the ${capability} capability; request it, or register an agent with it` });
      expect(ran).toEqual([]);
    }
  });

  it("is refused the same whether or not the task exists", async () => {
    const { send } = adapter(["a2a"], refuses(404, "task_not_found", "no such task in this project"));
    expect((await send("GetTask", { id: "task_ffffffffffffffffffff" })).status).toBe(403);
  });

  it("is not refused an operation that uses a capability it holds", async () => {
    const { ran, send } = adapter(["a2a", "queue_http"]);
    expect((await send("SendMessage", message({ url: "https://dest.example.com/hook" }))).body.result.task.id).toBe(TASK_ID);
    expect(ran).toHaveLength(1);
  });
});

describe("the payload schema", () => {
  const FIELDS = ["url", "method", "headers", "body", "run_at_ms", "delay", "idempotency_key", "max_attempts", "capture"];
  const input = (name: string) => capabilities.find((c) => c.name === name)!.input;
  const fetchSchema = async (env: Env = on) => worker.fetch(new Request(SCHEMA_URL), env, ctx);
  const rules = ({ description: _, ...rest }: Json) => rest;

  it("is served, versioned, at the URL the card's skill and the guidance name", async () => {
    const res = await fetchSchema();
    expect(res.status).toBe(200);
    expect(res.headers.get("Content-Type")).toContain("application/schema+json");
    expect(res.headers.get("Access-Control-Allow-Origin")).toBe("*");
    const schema = (await res.json()) as Json;
    expect(schema).toMatchObject({ $schema: "https://json-schema.org/draft/2020-12/schema", $id: SCHEMA_URL, type: "object", additionalProperties: false, required: ["url"] });
    const card = (await (await worker.fetch(new Request(`${ORIGIN}/.well-known/agent-card.json`), on, ctx)).json()) as Json;
    expect(card.skills[0].description).toContain(SCHEMA_URL);
  });

  it("has the decided fields and no other", async () => {
    const schema = (await (await fetchSchema()).json()) as Json;
    expect(Object.keys(schema.properties)).toEqual(FIELDS);
  });

  it("takes each field's rules from schedule_http, and they are queue_http's too", async () => {
    const schema = (await (await fetchSchema()).json()) as Json;
    for (const field of FIELDS) {
      expect(rules(schema.properties[field]), field).toEqual(rules(input("schedule_http").properties[field]!));
      const queued = input("queue_http").properties[field];
      if (queued) expect(rules(schema.properties[field]), field).toEqual(rules(queued));
      expect(typeof schema.properties[field].description === "string" || field === "method" || field === "max_attempts", field).toBe(true);
    }
    expect(schema.required).toEqual(input("schedule_http").required);
    // No description points an A2A caller at a capability it cannot call here.
    expect(JSON.stringify(schema)).not.toMatch(/queue_http|schedule_http|get_task|callback|inbox/);
  });

  it("is not served when A2A is off", async () => {
    expect((await fetchSchema({ ...on, A2A_ENABLED: "false" } as unknown as Env)).status).toBe(404);
  });
});
