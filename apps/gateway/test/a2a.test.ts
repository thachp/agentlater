import { describe, expect, it } from "vitest";
import { Caller, handleA2a, type Refusal } from "../src/a2a";
import worker, { type Env } from "../src/index";

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

    expect(card.documentationUrl).toMatch(/^https:\/\//);
    const [onboarding] = card.capabilities.extensions;
    // Data only: a client that ignores it can still call the agent.
    expect(onboarding.required ?? false).toBe(false);
    expect(onboarding.params).toEqual({
      discovery: "https://gateway.test/.well-known/agent-configuration",
      audience: "https://gateway.test/a2a",
      capability: "a2a",
    });
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
      expect(body).toMatchObject({ jsonrpc: "2.0", id: null, error: { code: -32009 } });
      expect(body.error.data).toEqual([{ "@type": "type.googleapis.com/google.rpc.ErrorInfo", reason: "VERSION_NOT_SUPPORTED", domain: "a2a-protocol.org" }]);
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

  // Sending and reading run capabilities: test/a2a-tasks.test.ts.
  it("does not list or cancel yet: both are unsupported", async () => {
    expect(await codeOf("CancelTask", { id: "task_1" })).toBe(-32004);
    expect(await codeOf("ListTasks")).toBe(-32004);
  });

  it("names the A2A error in the error's data", async () => {
    const body = await call(rpc("SubscribeToTask", { id: "task_1" }));
    expect(body.error.data).toEqual([{ "@type": "type.googleapis.com/google.rpc.ErrorInfo", reason: "UNSUPPORTED_OPERATION", domain: "a2a-protocol.org" }]);
  });

  it("answers JSON it cannot parse with a parse error", async () => {
    for (const body of ["{", "", "not json"]) expect(await call(body), body).toMatchObject({ jsonrpc: "2.0", id: null, error: { code: -32700 } });
  });

  it("answers JSON that is not a JSON-RPC request with invalid request", async () => {
    for (const body of ["{}", "[]", "null", '"GetTask"', '{"jsonrpc":"1.0","id":1,"method":"GetTask","params":{}}', '{"jsonrpc":"2.0","id":1,"params":{}}', `[${rpc("GetTask")}]`]) {
      expect(await call(body), body).toMatchObject({ jsonrpc: "2.0", id: null, error: { code: -32600 } });
    }
  });

  it("refuses a body over the gateway's limit", async () => {
    const res = await handleA2a(post(AUTH, rpc("SendMessage", { pad: "x".repeat(200 * 1024) })), on, accept, nothing);
    expect(res.status).toBe(413);
  });

  it("asks the verifier only about requests that carry a credential and speak 1.0", async () => {
    let asked = 0;
    const counting = async () => (asked++, accept());
    expect((await handleA2a(post(V1), on, counting, nothing)).status).toBe(401);
    expect((await handleA2a(post({ Authorization: "Bearer verified" }), on, counting, nothing)).status).toBe(200);
    expect(asked).toBe(0);
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
});

describe("A2A_ENABLED in wrangler.jsonc", () => {
  it("is on in staging and off in production", async () => {
    const { readFileSync } = await import("node:fs");
    const { dirname, join } = await import("node:path");
    const { fileURLToPath } = await import("node:url");
    const config = readFileSync(join(dirname(fileURLToPath(import.meta.url)), "../wrangler.jsonc"), "utf8");
    const [beforeProduction, production] = config.split('"production": {') as [string, string];
    const staging = beforeProduction.split('"staging": {')[1];
    expect(staging).toContain('"A2A_ENABLED": "true"');
    expect(production).not.toContain('"A2A_ENABLED": "true"');
  });
});
