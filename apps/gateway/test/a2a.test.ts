import { describe, expect, it, vi } from "vitest";
import { Caller, serveA2a, type Refusal } from "../src/a2a";
import { ApiError } from "../src/api";
import worker, { type Env } from "../src/index";

/** /a2a itself, without the rest of the gateway: the verifier and the capabilities are the test's. */
const handleA2a = async (...args: Parameters<typeof serveA2a>) => (await serveA2a(...args))!;

const ctx = { waitUntil() {}, passThroughOnException() {} } as unknown as ExecutionContext;
const SECRETS = { ASSERTION_PRIVATE_KEY: "07".repeat(32), BETTER_AUTH_SECRET: "better-auth-secret-value", STRIPE_SECRET_KEY: "sk_test_value" };
const on = { PUBLIC_URL: "https://gateway.test", ALT_PUBLIC_URL: "https://gateway.other.test", A2A_ENABLED: "true", ...SECRETS } as unknown as Env;
const get = (url: string, env: Env = on) => worker.fetch(new Request(url), env, ctx);

type Json = Record<string, any>;
const cardAt = async (origin: string) => {
  const res = await get(`${origin}/.well-known/agent-card.json`);
  expect(res.status).toBe(200);
  return { res, text: await res.clone().text(), card: (await res.json()) as Json };
};

/**
 * Fields marked REQUIRED in the pinned protocol definition
 * (https://github.com/a2aproject/A2A/blob/v1.0.1/specification/a2a.proto), by message. A required
 * list must have at least one element (specification 5.7).
 */
const REQUIRED = {
  AgentCard: ["name", "description", "supportedInterfaces", "version", "capabilities", "defaultInputModes", "defaultOutputModes", "skills"],
  AgentInterface: ["url", "protocolBinding", "protocolVersion"],
  AgentProvider: ["url", "organization"],
  AgentSkill: ["id", "name", "description", "tags"],
  HTTPAuthSecurityScheme: ["scheme"],
};
function expectRequired(message: keyof typeof REQUIRED, value: Json) {
  for (const field of REQUIRED[message]) {
    const present = Array.isArray(value[field]) ? value[field].length > 0 : typeof value[field] === "object" ? value[field] !== null : Boolean(value[field]);
    expect(present, `${message}.${field}`).toBe(true);
  }
}

describe("Agent Card", () => {
  it("loads without credentials and declares JSON-RPC 1.0 at /a2a", async () => {
    const { card } = await cardAt("https://gateway.test");
    expect(card.supportedInterfaces).toEqual([{ url: "https://gateway.test/a2a", protocolBinding: "JSONRPC", protocolVersion: "1.0" }]);
  });

  it("has every field the protocol requires, and none it does not define at the top level", async () => {
    const { card } = await cardAt("https://gateway.test");
    expectRequired("AgentCard", card);
    for (const i of card.supportedInterfaces) expectRequired("AgentInterface", i);
    expectRequired("AgentProvider", card.provider);
    for (const skill of card.skills) expectRequired("AgentSkill", skill);
    // 1.0 has no top-level url, protocolVersion or security; a 0.3 card did.
    const defined = [...REQUIRED.AgentCard, "provider", "documentationUrl", "securitySchemes", "securityRequirements", "signatures", "iconUrl"];
    expect(Object.keys(card).filter((k) => !defined.includes(k))).toEqual([]);
  });

  it("turns streaming, push notifications and the extended card off", async () => {
    const { card } = await cardAt("https://gateway.test");
    expect(card.capabilities).toMatchObject({ streaming: false, pushNotifications: false, extendedAgentCard: false });
  });

  it("requires a bearer JWT and points at Agent Auth onboarding", async () => {
    const { card } = await cardAt("https://gateway.test");
    expect(Object.keys(card.securitySchemes)).toHaveLength(1);
    const name = Object.keys(card.securitySchemes)[0] as string;
    const scheme = card.securitySchemes[name].httpAuthSecurityScheme;
    expectRequired("HTTPAuthSecurityScheme", scheme);
    expect(scheme).toMatchObject({ scheme: "Bearer", bearerFormat: "JWT" });
    // The 1.0 form: a list of { schemes: { name: scopes } }, here with no scopes.
    expect(card.securityRequirements).toHaveLength(1);
    expect(Object.keys(card.securityRequirements[0].schemes)).toEqual([name]);
    expect(card.securityRequirements[0].schemes[name].list ?? []).toEqual([]);

    expect(card.documentationUrl).toBe("https://agentlater.io/docs/a2a");
    const [onboarding] = card.capabilities.extensions;
    // Data only: a client that ignores it can still call the agent.
    expect(onboarding.required ?? false).toBe(false);
    expect(onboarding.params).toEqual({
      discovery: "https://gateway.test/.well-known/agent-configuration",
      audience: "https://gateway.test/a2a",
      capability: "a2a",
      // What else the agent must hold, by the operation that runs it.
      operations: { SendMessage: ["schedule_http", "queue_http"], GetTask: ["get_task"], ListTasks: ["list_tasks"], CancelTask: ["cancel_task"] },
    });
    expect(onboarding.description).toContain("`operations`");
    expect(scheme.description).toContain(
      "The agent must also hold the capability each operation runs: SendMessage schedule_http for a request with run_at_ms or delay and queue_http for one with neither; " +
        "GetTask get_task; ListTasks list_tasks; CancelTask cancel_task.",
    );
  });

  it("lists delivery of an HTTP request as its only skill, taking and returning JSON", async () => {
    const { card } = await cardAt("https://gateway.test");
    expect(card.skills.map((s: Json) => s.id)).toEqual(["deliver-http"]);
    expect(card.defaultInputModes).toEqual(["application/json"]);
    expect(card.defaultOutputModes).toEqual(["application/json"]);
    // The example is a payload a client could send: only fields the delivery payload will accept.
    const accepted = ["url", "method", "headers", "body", "run_at_ms", "delay", "idempotency_key", "max_attempts", "capture"];
    for (const example of card.skills[0].examples) {
      expect(Object.keys(JSON.parse(example)).filter((k) => !accepted.includes(k))).toEqual([]);
    }
  });

  it("advertises nothing this version leaves out, and neither does the payload schema", async () => {
    const { text } = await cardAt("https://gateway.test");
    const schema = await (await get("https://gateway.test/a2a/schemas/deliver-http/v1.json")).text();
    // queue_http is named as the capability SendMessage runs for a request sent now; a queue to send in is not offered.
    for (const published of [text, schema]) expect(published).not.toMatch(/cron|watch|callback|lane|queue(?!_http)|inbox|wake|set_secret|payment|redrive|stream(?!ing":false)/i);
  });

  it("contains no secret", async () => {
    const { text } = await cardAt("https://gateway.test");
    for (const secret of Object.values(SECRETS)) expect(text).not.toContain(secret);
  });

  it("names the host that was asked", async () => {
    const { text, card } = await cardAt("https://gateway.other.test");
    expect(card.supportedInterfaces[0].url).toBe("https://gateway.other.test/a2a");
    expect(card.capabilities.extensions[0].params.discovery).toBe("https://gateway.other.test/.well-known/agent-configuration");
    expect(text).not.toContain("gateway.test");
  });

  it("can be cached and read from a browser", async () => {
    const { res } = await cardAt("https://gateway.test");
    expect(res.headers.get("Cache-Control")).toContain("max-age=");
    expect(res.headers.get("Access-Control-Allow-Origin")).toBe("*");
  });
});

const rpc = (method: string, params: unknown = {}) => JSON.stringify({ jsonrpc: "2.0", id: 7, method, params });
const post = (headers: Record<string, string>, body = rpc("GetTask", { id: "task_1" })) =>
  new Request("https://gateway.test/a2a", { method: "POST", headers: { "Content-Type": "application/json", ...headers }, body });
const V1 = { "A2A-Version": "1.0" };
/** Stands in for running a capability in tests where none may run. */
const nothing = async (): Promise<never> => {
  throw new Error("no capability runs here");
};
const CHALLENGE = 'AgentAuth discovery="https://gateway.test/.well-known/agent-configuration"';

describe("/a2a through the gateway", () => {
  it("refuses a request without credentials with the Agent Auth discovery challenge", async () => {
    const res = await worker.fetch(post(V1), on, ctx);
    expect(res.status).toBe(401);
    expect(res.headers.get("WWW-Authenticate")).toBe(CHALLENGE);
    expect(await res.json()).toMatchObject({ error: "authentication_required" });
  });

  it("answers a request without A2A-Version: 1.0 with VersionNotSupportedError, credentials or not", async () => {
    const others: Record<string, string>[] = [{}, { "A2A-Version": "0.3" }, { "A2A-Version": "2.0", Authorization: "Bearer anything" }];
    for (const headers of others) {
      const res = await worker.fetch(post(headers), on, ctx);
      expect(res.status).toBe(200);
      const body = (await res.json()) as Json;
      expect(body).toMatchObject({ jsonrpc: "2.0", error: { code: -32009 } });
      expect(body.error.data).toEqual([{ "@type": "type.googleapis.com/google.rpc.ErrorInfo", reason: "VERSION_NOT_SUPPORTED", domain: "a2a-protocol.org" }]);
    }
  });

  it("answers a version it does not speak by the request's id, and by null when the id cannot be read", async () => {
    const answerTo = async (body: string) => (await (await worker.fetch(post({ "A2A-Version": "0.3" }, body), on, ctx)).json()) as Json;
    expect(await answerTo(rpc("GetTask"))).toMatchObject({ id: 7, error: { code: -32009 } });
    expect(await answerTo('{"jsonrpc":"2.0","id":"abc","method":"GetTask"}')).toMatchObject({ id: "abc", error: { code: -32009 } });
    for (const body of ["{", "", "[]", '{"id":{"a":1}}', '{"jsonrpc":"2.0","id":null,"method":"GetTask"}', JSON.stringify({ id: 7, pad: "x".repeat(200 * 1024) })]) {
      expect(await answerTo(body), body.slice(0, 40)).toMatchObject({ id: null, error: { code: -32009 } });
    }
  });

  it("takes a version with a patch number as 1.0, and refuses every other version", async () => {
    // Accepted versions reach the credential check; the others never do.
    for (const version of ["1.0.1", "1.0.7", "1.0.0"]) {
      expect((await worker.fetch(post({ "A2A-Version": version }), on, ctx)).status, version).toBe(401);
      expect((await worker.fetch(new Request(`https://gateway.test/a2a?A2A-Version=${version}`, { method: "POST", body: rpc("GetTask") }), on, ctx)).status, version).toBe(401);
    }
    for (const version of ["1.1.0", "1.1", "0.3.0", "2.0.1", "1", "1.0.1.2", "1.0.x", "1.0.", "v1.0.1"]) {
      const res = await worker.fetch(post({ "A2A-Version": version }), on, ctx);
      expect(res.status, version).toBe(200);
      expect(await res.json(), version).toMatchObject({ error: { code: -32009 } });
    }
  });

  it("takes the version from the query when no header carries it", async () => {
    const res = await worker.fetch(new Request("https://gateway.test/a2a?A2A-Version=1.0", { method: "POST", body: rpc("GetTask") }), on, ctx);
    expect(res.status).toBe(401);
  });

  it("answers only POST", async () => {
    const res = await get("https://gateway.test/a2a");
    expect(res.status).toBe(405);
    expect(res.headers.get("Allow")).toBe("POST");
    expect(res.headers.get("Cache-Control")).toBe("no-store");
  });
});

describe("/a2a for a caller the verifier refuses", () => {
  const AUTH = { ...V1, Authorization: "Bearer refused" };
  const refuse = (refusal: Refusal) => handleA2a(post(AUTH), on, async () => refusal, nothing);

  it("answers 401 with the Agent Auth discovery challenge for a token that is not good here", async () => {
    const res = await refuse({ status: 401, code: "invalid_jwt", message: "no" });
    expect(res.status).toBe(401);
    expect(res.headers.get("WWW-Authenticate")).toBe(CHALLENGE);
    expect(await res.json()).toEqual({ error: "invalid_jwt", message: "no" });
  });

  it("answers 403 without a challenge for a caller that may not use /a2a", async () => {
    const res = await refuse({ status: 403, code: "capability_not_granted", message: "no" });
    expect(res.status).toBe(403);
    expect(res.headers.get("WWW-Authenticate")).toBeNull();
    expect(await res.json()).toEqual({ error: "capability_not_granted", message: "no" });
  });

  it("answers 429 and says when to come back for a caller over the rate limit", async () => {
    const res = await refuse({ status: 429, code: "rate_limited", message: "slow down", retryAfter: "17" });
    expect(res.status).toBe(429);
    expect(res.headers.get("Retry-After")).toBe("17");
  });

  it("never reads the body or reaches JSON-RPC", async () => {
    const request = post(AUTH, "{");
    const res = await handleA2a(request, on, async () => ({ status: 401, code: "invalid_jwt", message: "no" }), nothing);
    expect(await res.json()).not.toHaveProperty("jsonrpc");
    expect(request.bodyUsed).toBe(false);
  });
});

/** What /a2a answers once the verifier says who is calling. */
describe("/a2a for a verified caller", () => {
  const accept = async () => new Caller("host_1", new Set(["a2a"]));
  const AUTH = { ...V1, Authorization: "Bearer verified" };
  const call = async (body: string) => {
    const res = await handleA2a(post(AUTH, body), on, accept, nothing);
    expect(res.status).toBe(200);
    return (await res.json()) as Json;
  };
  const codeOf = async (method: string, params: unknown = {}) => {
    const body = await call(rpc(method, params));
    expect(body, method).toMatchObject({ jsonrpc: "2.0", id: 7 });
    expect(body.result, method).toBeUndefined();
    return body.error.code as number;
  };

  it("answers streaming and the extended card with UnsupportedOperationError", async () => {
    for (const method of ["SendStreamingMessage", "SubscribeToTask", "GetExtendedAgentCard"]) expect(await codeOf(method), method).toBe(-32004);
  });

  it("answers push notification config methods with PushNotificationNotSupportedError", async () => {
    for (const method of ["CreateTaskPushNotificationConfig", "GetTaskPushNotificationConfig", "ListTaskPushNotificationConfigs", "DeleteTaskPushNotificationConfig"]) {
      expect(await codeOf(method), method).toBe(-32003);
    }
  });

  it("answers unknown methods, the 0.3 names among them, with method not found", async () => {
    for (const method of ["Nope", "message/send", "tasks/get", "getTask"]) expect(await codeOf(method), method).toBe(-32601);
  });

  it("names the A2A error in the error's data", async () => {
    const body = await call(rpc("SubscribeToTask", { id: "task_1" }));
    expect(body.error.data).toEqual([{ "@type": "type.googleapis.com/google.rpc.ErrorInfo", reason: "UNSUPPORTED_OPERATION", domain: "a2a-protocol.org" }]);
  });

  it("answers JSON it cannot parse with a parse error", async () => {
    for (const body of ["{", "", "not json"]) expect(await call(body), body).toMatchObject({ jsonrpc: "2.0", id: null, error: { code: -32700 } });
  });

  it("answers JSON that is not a JSON-RPC request with invalid request", async () => {
    for (const body of ["{}", "[]", "null", '"GetTask"', '{"jsonrpc":"1.0","method":"GetTask","params":{}}', '{"jsonrpc":"2.0","params":{}}', `[${rpc("GetTask")}]`]) {
      expect(await call(body), body).toMatchObject({ jsonrpc: "2.0", id: null, error: { code: -32600 } });
    }
  });

  it("answers an id that is not a string, a number or null with invalid request, by null, and runs nothing", async () => {
    let ran = 0;
    const holder = async () => new Caller("host_1", new Set(["a2a", "get_task"]));
    for (const id of ['{"a":1}', "[1]", "true", "false"]) {
      const res = await handleA2a(post(AUTH, `{"jsonrpc":"2.0","id":${id},"method":"GetTask","params":{"id":"task_1"}}`), on, holder, async () => (ran++, {}));
      expect(await res.json(), id).toEqual({ jsonrpc: "2.0", id: null, error: { code: -32600, message: "Request payload validation error" } });
    }
    expect(ran).toBe(0);
  });

  it("answers every error by the request's id, and by null only when the id could not be read", async () => {
    // Not a JSON-RPC 2.0 request, but its id can be read.
    expect(await call('{"jsonrpc":"1.0","id":1,"method":"GetTask","params":{}}')).toMatchObject({ id: 1, error: { code: -32600 } });
    expect(await call('{"jsonrpc":"2.0","id":"abc","params":{}}')).toMatchObject({ id: "abc", error: { code: -32600 } });
    expect(await call('{"jsonrpc":"2.0","id":"abc","method":"Nope","params":{}}')).toMatchObject({ id: "abc", error: { code: -32601 } });
    expect(await call('{"jsonrpc":"2.0","id":0,"method":"GetTask","params":[]}')).toMatchObject({ id: 0, error: { code: -32602 } });
    // What the API refuses for a reason that is not the request's.
    const refused = async () => {
      throw new ApiError(429, { error: { code: "rate_limited", message: "slow down" } });
    };
    const res = await handleA2a(post(AUTH, rpc("ListTasks")), on, async () => new Caller("host_1", new Set(["a2a", "list_tasks"])), refused);
    expect(await res.json()).toMatchObject({ id: 7, error: { code: -32000 } });
    expect(await call("{")).toMatchObject({ id: null, error: { code: -32700 } });
  });

  it("does not answer a notification, a request with no id, and runs nothing for it", async () => {
    const message = { messageId: "m-1", role: "ROLE_USER", parts: [{ data: { url: "https://dest.example.com/hook" }, mediaType: "application/json" }] };
    const everything = async () => new Caller("host_1", new Set(["a2a", "queue_http", "get_task", "cancel_task"]));
    let ran = 0;
    const counting = async () => (ran++, {});
    for (const [method, params] of [["SendMessage", { message }], ["CancelTask", { id: "task_1" }], ["GetTask", { id: "task_1" }], ["Nope", {}]] as const) {
      const res = await handleA2a(post(AUTH, JSON.stringify({ jsonrpc: "2.0", method, params })), on, everything, counting);
      expect(res.status, method).toBe(204);
      expect(await res.text(), method).toBe("");
    }
    expect(ran).toBe(0);
    // In a version that is not spoken, too.
    const other = await handleA2a(post({ "A2A-Version": "0.3" }, JSON.stringify({ jsonrpc: "2.0", method: "GetTask", params: {} })), on, everything, counting);
    expect(other.status).toBe(204);
    expect(await other.text()).toBe("");
    // A null id is an id: that request is answered.
    expect(await call('{"jsonrpc":"2.0","id":null,"method":"Nope","params":{}}')).toMatchObject({ id: null, error: { code: -32601 } });
    // A caller without credentials is still turned away at HTTP.
    expect((await handleA2a(post(V1, JSON.stringify({ jsonrpc: "2.0", method: "GetTask" })), on, everything, counting)).status).toBe(401);
  });

  it("refuses a body over the gateway's limit", async () => {
    const res = await handleA2a(post(AUTH, rpc("SendMessage", { pad: "x".repeat(200 * 1024) })), on, accept, nothing);
    expect(res.status).toBe(413);
    expect(res.headers.get("Cache-Control")).toBe("no-store");
  });

  it("asks the verifier only about requests that carry a credential and speak 1.0", async () => {
    let asked = 0;
    const counting = async () => (asked++, accept());
    expect((await handleA2a(post(V1), on, counting, nothing)).status).toBe(401);
    expect((await handleA2a(post({ Authorization: "Bearer verified" }), on, counting, nothing)).status).toBe(200);
    expect(asked).toBe(0);
  });
});

describe("the log line of an /a2a request", () => {
  const TOKEN = "eyJhbGciOiJFZERTQSJ9.eyJzdWIiOiJhZ2VudF8xIiwianRpIjoiYWJjIn0.c2lnbmF0dXJlLXNlY3JldA";
  const AUTH = { ...V1, Authorization: `Bearer ${TOKEN}`, "CF-Ray": "8f2c1d3e4a5b6c7d-SJC" };
  const accept = async () => new Caller("host_1", new Set(["a2a", "schedule_http"]));
  const scheduled = async () => ({ id: "task_0123456789abcdef0123", status: "scheduled", updated_ms: 1_800_000_000_000, capture: false, response: null });
  /** Sends one request and returns what it wrote with console.log: each line parsed, and all of them as text. */
  async function logged(request: Request, verify: Parameters<typeof serveA2a>[2] = accept, run: Parameters<typeof serveA2a>[3] = scheduled) {
    const log = vi.spyOn(console, "log").mockImplementation(() => {});
    try {
      await handleA2a(request, on, verify, run);
      const text = log.mock.calls.map((line) => line.join(" ")).join("\n");
      return { lines: log.mock.calls.map(([line]) => JSON.parse(line as string) as Json), text };
    } finally {
      log.mockRestore();
    }
  }
  /** What the one line of a request says of how it ended. */
  async function ending(...args: Parameters<typeof logged>) {
    const { lines } = await logged(...args);
    expect(lines).toHaveLength(1);
    const { method, outcome, status, host } = lines[0]!;
    return { method, outcome, status, host };
  }

  it("is one JSON line with the method, the outcome, the HTTP status, the duration, the ray id and the host, and nothing the caller sent", async () => {
    const payload = { url: "https://dest.example.com/hook?key=url-secret-9f8e", headers: { Authorization: "Bearer payload-secret-7c6b" }, body: "body-secret-5a4f", delay: "15m" };
    const message = { messageId: "message-id-3e2d", role: "ROLE_USER", parts: [{ data: payload, mediaType: "application/json" }] };
    const { lines, text } = await logged(post({ ...AUTH, "X-Api-Key": "header-secret-4d1a" }, rpc("SendMessage", { message })));
    expect(lines).toEqual([{ event: "a2a_request", method: "SendMessage", outcome: "ok", status: 200, ms: expect.any(Number), ray: "8f2c1d3e4a5b6c7d-SJC", host: "host_1" }]);
    expect(lines[0]!.ms).toBeGreaterThanOrEqual(0);
    for (const secret of [TOKEN, ...TOKEN.split("."), "url-secret-9f8e", "payload-secret-7c6b", "body-secret-5a4f", "header-secret-4d1a", "message-id-3e2d", "dest.example.com", "Bearer"]) {
      expect(text).not.toContain(secret);
    }
  });

  it("says how the request ended: ok, a JSON-RPC error's code, or the code of a refusal at HTTP", async () => {
    expect(await ending(post(AUTH, rpc("SubscribeToTask", { id: "task_1" })))).toEqual({ method: "SubscribeToTask", outcome: -32004, status: 200, host: "host_1" });
    expect(await ending(post(AUTH, "{"))).toEqual({ method: null, outcome: -32700, status: 200, host: "host_1" });
    expect(await ending(post({ Authorization: AUTH.Authorization }))).toEqual({ method: null, outcome: -32009, status: 200, host: null });
    expect(await ending(post(AUTH, JSON.stringify({ jsonrpc: "2.0", method: "GetTask", params: { id: "task_1" } })))).toEqual({ method: "GetTask", outcome: "notification", status: 204, host: "host_1" });
    expect(await ending(post(V1))).toEqual({ method: null, outcome: "authentication_required", status: 401, host: null });
    expect(await ending(new Request("https://gateway.test/a2a"))).toEqual({ method: null, outcome: "method_not_allowed", status: 405, host: null });
    expect(await ending(post(AUTH, rpc("SendMessage", { pad: "x".repeat(200 * 1024) })))).toEqual({ method: null, outcome: "too_large", status: 413, host: "host_1" });
    // The agent holds a2a and schedule_http only.
    expect(await ending(post(AUTH, rpc("GetTask", { id: "task_0123456789abcdef0123" })))).toEqual({ method: "GetTask", outcome: "capability_not_granted", status: 403, host: "host_1" });
  });

  it("leaves out a method name, a refusal's code and a ray id that are not plain words, and every error's own text", async () => {
    const odd = await logged(post({ ...AUTH, "CF-Ray": "ray with spaces" }, rpc(`tasks/get ${TOKEN}`)));
    expect(odd.lines).toMatchObject([{ method: null, outcome: -32601, ray: null }]);
    expect(odd.text).not.toContain("tasks/get");

    const refused = await logged(post(AUTH), async () => ({ status: 401, code: "invalid_jwt", message: `the token ${TOKEN} is not accepted` }));
    expect(refused.lines).toMatchObject([{ outcome: "invalid_jwt", status: 401, host: null }]);
    expect(refused.text).not.toContain("not accepted");
    const sentence = await logged(post(AUTH), async () => ({ status: 401, code: `no: ${TOKEN}`, message: "no" }));
    expect(sentence.lines).toMatchObject([{ outcome: null, status: 401 }]);

    const message = { messageId: "m-1", role: "ROLE_USER", parts: [{ data: { url: "https://dest.example.com/hook", delay: "15m" }, mediaType: "application/json" }] };
    const broken = await logged(post(AUTH, rpc("SendMessage", { message })), accept, async () => {
      throw new TypeError("failure-detail-1b2c");
    });
    expect(broken.lines).toMatchObject([{ method: "SendMessage", outcome: -32603, status: 200 }]);
    for (const { text } of [odd, refused, sentence, broken]) for (const secret of [TOKEN, "failure-detail-1b2c"]) expect(text).not.toContain(secret);
  });
});

describe("an operation that fails inside AgentLater", () => {
  it("is found by its request's log line, and nothing its error says is in any log line: no URL, header value or token", async () => {
    const TOKEN = "eyJhbGciOiJFZERTQSJ9.eyJzdWIiOiJhZ2VudF8xIiwianRpIjoiYWJjIn0.c2lnbmF0dXJlLXNlY3JldA";
    const methods = ["log", "info", "warn", "error", "debug"] as const;
    const spies = methods.map((name) => vi.spyOn(console, name).mockImplementation(() => {}));
    try {
      const message = { messageId: "m-1", role: "ROLE_USER", parts: [{ data: { url: "https://dest.example.com/hook", delay: "15m" }, mediaType: "application/json" }] };
      const request = post({ ...V1, Authorization: `Bearer ${TOKEN}`, "CF-Ray": "8f2c1d3e4a5b6c7d-SJC" }, rpc("SendMessage", { message }));
      const res = await handleA2a(request, on, async () => new Caller("host_1", new Set(["a2a", "schedule_http"])), async () => {
        throw new TypeError(`POST https://dest.example.com/hook?key=url-secret-9f8e with Authorization: Bearer header-secret-4d1a failed for ${TOKEN}`);
      });
      expect(await res.json()).toMatchObject({ error: { code: -32603, message: "AgentLater could not complete this request; try again" } });
      const lines = spies.flatMap((spy) => spy.mock.calls.map((call) => call.map(String).join(" ")));
      expect(lines.map((line) => JSON.parse(line))).toEqual([
        { event: "a2a_request", method: "SendMessage", outcome: -32603, status: 200, ms: expect.any(Number), ray: "8f2c1d3e4a5b6c7d-SJC", host: "host_1" },
      ]);
      for (const secret of [TOKEN, "url-secret-9f8e", "header-secret-4d1a", "dest.example.com", "Bearer", "failed for"]) expect(lines.join("\n")).not.toContain(secret);
    } finally {
      for (const spy of spies) spy.mockRestore();
    }
  });
});

describe("with A2A_ENABLED off", () => {
  it("answers 404 for the card and for /a2a", async () => {
    for (const value of [undefined, "false", ""]) {
      const off = { ...on, A2A_ENABLED: value } as unknown as Env;
      expect((await get("https://gateway.test/.well-known/agent-card.json", off)).status, String(value)).toBe(404);
      expect((await worker.fetch(post({ ...V1, Authorization: "Bearer anything" }), off, ctx)).status, String(value)).toBe(404);
      expect((await worker.fetch(post({}), off, ctx)).status, String(value)).toBe(404);
    }
  });

  it("leaves A2A out of llms.txt, which names the card only where it is served", async () => {
    const llms = async (env: Env) => (await get("https://gateway.test/llms.txt", env)).text();
    expect(await llms({ ...on, A2A_ENABLED: "false" } as unknown as Env)).not.toMatch(/a2a/i);
    const served = await llms(on);
    expect(served).toContain("https://gateway.test/.well-known/agent-card.json");
    expect(served).toContain("https://agentlater.io/docs/a2a");
  });
});

describe("A2A_ENABLED in wrangler.jsonc", () => {
  it("is on in staging and in production", async () => {
    const { readFileSync } = await import("node:fs");
    const { dirname, join } = await import("node:path");
    const { fileURLToPath } = await import("node:url");
    const config = readFileSync(join(dirname(fileURLToPath(import.meta.url)), "../wrangler.jsonc"), "utf8");
    const [beforeProduction, production] = config.split('"production": {') as [string, string];
    const staging = beforeProduction.split('"staging": {')[1];
    expect(staging).toContain('"A2A_ENABLED": "true"');
    expect(production).toContain('"A2A_ENABLED": "true"');
  });
});
