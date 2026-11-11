import { describe, expect, it } from "vitest";
import worker, { type Env } from "../src/index";

const envWithApi = (status: number) =>
  ({ PUBLIC_URL: "https://gateway.test", API: { fetch: async () => new Response(null, { status }) } }) as unknown as Env;
const ctx = { waitUntil() {}, passThroughOnException() {} } as unknown as ExecutionContext;

const get = (path: string, env: Env) => worker.fetch(new Request(`https://gateway.test${path}`), env, ctx);

describe("gateway", () => {
  it("reports healthy when the API is reachable", async () => {
    const res = await get("/health", envWithApi(200));
    expect(res.status).toBe(200);
    expect(await res.json()).toMatchObject({ status: "ok", api: "ok" });
  });

  it("reports 503 when the API is down", async () => {
    const res = await get("/health", envWithApi(500));
    expect(res.status).toBe(503);
  });

  it("rate limits registration per IP", async () => {
    const env = { ...envWithApi(200), REGISTER_RATE: { limit: async () => ({ success: false }) } } as unknown as Env;
    const req = new Request("https://gateway.test/api/auth/agent/register", {
      method: "POST",
      headers: { "CF-Connecting-IP": "203.0.113.7" },
      body: "{}",
    });
    const res = await worker.fetch(req, env, ctx);
    expect(res.status).toBe(429);
    expect(await res.json()).toMatchObject({ error: "rate_limited" });
  });

  it("refuses oversized bodies before parsing them", async () => {
    const req = new Request("https://gateway.test/api/auth/capability/execute", {
      method: "POST",
      headers: { "Content-Length": String(1024 * 1024) },
      body: "{}",
    });
    expect((await worker.fetch(req, envWithApi(200), ctx)).status).toBe(413);
  });

  it("counts body bytes even without Content-Length", async () => {
    const chunk = new Uint8Array(64 * 1024);
    let sent = 0;
    const stream = new ReadableStream<Uint8Array>({
      pull(c) {
        if (sent++ < 4) c.enqueue(chunk);
        else c.close();
      },
    });
    const req = new Request("https://gateway.test/api/auth/capability/execute", {
      method: "POST",
      body: stream,
      duplex: "half",
    } as RequestInit);
    expect(req.headers.get("Content-Length")).toBeNull();
    expect((await worker.fetch(req, envWithApi(200), ctx)).status).toBe(413);
  });

  it("claims each token's jti atomically before Better Auth sees it", async () => {
    const claims: string[] = [];
    const store = { claim: async (key: string) => (claims.push(key), claims.length === 1) };
    const env = {
      ...envWithApi(200),
      ASSERTION_PRIVATE_KEY: "07".repeat(32),
      AUTH_STORE: { idFromName: () => "id", get: () => store },
    } as unknown as Env;
    const b64 = (o: object) => btoa(JSON.stringify(o)).replace(/=+$/, "");
    const token = `${b64({ alg: "EdDSA" })}.${b64({ sub: "agent_1", jti: "abc" })}.sig`;
    const send = () =>
      worker.fetch(
        new Request("https://gateway.test/api/auth/capability/execute", {
          method: "POST",
          headers: { Authorization: `Bearer ${token}` },
          body: "{}",
        }),
        env,
        ctx,
      );
    // First use passes the gateway (and then fails inside Better Auth, which has no DB here).
    await send().catch(() => undefined);
    const second = await send();
    expect(second.status).toBe(401);
    expect(await second.json()).toMatchObject({ error: "invalid_jwt" });
    expect(claims).toEqual(["replay::agent_1:abc", "replay::agent_1:abc"]);
  });

  describe("a token on an Agent Auth route", () => {
    const part = (o: object) => Buffer.from(JSON.stringify(o)).toString("base64url");
    const jwt = (claims: object) => `${part({ alg: "EdDSA" })}.${part(claims)}.c2ln`;
    /**
     * Sends a request and says what the gateway claimed in the AuthStore and what it answered. Better
     * Auth has no database here, so a request the gateway passes on fails there: "passed on".
     */
    async function send(authorization: string | undefined, path = "/api/auth/capability/execute") {
      const claims: string[] = [];
      const env = {
        ...envWithApi(200),
        ASSERTION_PRIVATE_KEY: "07".repeat(32),
        REGISTER_RATE: { limit: async () => ({ success: true }) },
        AUTH_STORE: { idFromName: () => "id", get: () => ({ claim: async (key: string) => (claims.push(key), true) }) },
      } as unknown as Env;
      const request = new Request(`https://gateway.test${path}`, { method: "POST", headers: authorization === undefined ? {} : { Authorization: authorization }, body: "{}" });
      const response = await worker.fetch(request, env, ctx).catch(() => undefined);
      if (response?.status !== 401) return { status: "passed on", claims };
      return { status: 401, body: await response.json(), claims };
    }
    const MALFORMED = { error: "invalid_jwt", message: "the Authorization header must be Bearer, one space and a compact JWT" };

    it("is passed on when the header is Bearer, one space and a compact JWT, and when there is no header", async () => {
      const token = jwt({ sub: "agent_1", jti: "abc" });
      expect(await send(`Bearer ${token}`)).toMatchObject({ status: "passed on", claims: ["replay::agent_1:abc"] });
      expect(await send(`bearer ${token}`)).toMatchObject({ status: "passed on" });
      expect(await send(undefined)).toMatchObject({ status: "passed on", claims: [] });
    });

    it("is refused before Better Auth and before any claim when the header is anything else", async () => {
      const token = jwt({ sub: "agent_1", jti: "abc" });
      const [header, payload, signature] = token.split(".");
      const others = [
        `Bearer ${header}.${payload}.c2 ln`,
        `Bearer ${header}.${payload}.c2\tln`,
        `Bearer ${header}.${payload}. ${signature}`,
        `Bearer  ${token}`,
        `Bearer\t${token}`,
        `Bearer ${token}=`,
        `Bearer ${token} =`,
        `Bearer ${token}.extra`,
        `Bearer ${header}.${payload}`,
        `Bearer ${header}..${signature}`,
        `Bearer ${header}.${payload}.c2+n`,
        `Bearer ${token}, Bearer ${token}`,
        `Token ${token}`,
        "Basic dXNlcjpwYXNz",
        "Bearer opaque-token",
        "Bearer",
        "",
      ];
      for (const authorization of others) {
        for (const path of ["/api/auth/capability/execute", "/api/auth/agent/register", "/api/auth/capability/list", "/api/auth/host/revoke"]) {
          expect(await send(authorization, path), `${JSON.stringify(authorization)} on ${path}`).toEqual({ status: 401, body: MALFORMED, claims: [] });
        }
      }
    });

    it("is refused when its claims cannot be read or its jti is not a non-empty string", async () => {
      const unreadable = [`${part({ alg: "EdDSA" })}.bm90LWpzb24.c2ln`, `${part({ alg: "EdDSA" })}.${Buffer.from('"a string"').toString("base64url")}.c2ln`, jwt([1])];
      const noJti = [jwt({ sub: "agent_1" }), jwt({ sub: "agent_1", jti: "" }), jwt({ sub: "agent_1", jti: 7 }), jwt({ sub: "agent_1", jti: true }), jwt({ sub: "agent_1", jti: ["abc"] }), jwt({ sub: "agent_1", jti: null })];
      for (const token of [...unreadable, ...noJti]) {
        expect(await send(`Bearer ${token}`), token).toEqual({ status: 401, body: { error: "invalid_jwt", message: "this token has no jti" }, claims: [] });
      }
    });

    it("is claimed by a digest when its jti, issuer and subject are too long to store as they are", async () => {
      const long = "j".repeat(5_000);
      const first = await send(`Bearer ${jwt({ sub: "agent_1", jti: long })}`);
      expect(first.status).toBe("passed on");
      expect(first.claims).toHaveLength(1);
      expect(first.claims[0]).toMatch(/^replay:sha256:[0-9a-f]{64}$/);
      // The same token claims the same key, and another jti, subject or issuer a different one.
      expect((await send(`Bearer ${jwt({ sub: "agent_1", jti: long })}`)).claims).toEqual(first.claims);
      for (const other of [{ sub: "agent_1", jti: `${long}k` }, { sub: "agent_2", jti: long }, { iss: "host_1", sub: "agent_1", jti: long }]) {
        const { claims } = await send(`Bearer ${jwt(other)}`);
        expect(claims[0]).toMatch(/^replay:sha256:/);
        expect(claims).not.toEqual(first.claims);
      }
      // A jti of ordinary length is claimed as it is, as before.
      expect((await send(`Bearer ${jwt({ sub: "agent_1", jti: "j".repeat(128) })}`)).claims).toEqual([`replay::agent_1:${"j".repeat(128)}`]);
      expect((await send(`Bearer ${jwt({ sub: "agent_1", jti: "j".repeat(129) })}`)).claims[0]).toMatch(/^replay:sha256:/);
    });
  });

  it("serves only the agent routes", async () => {
    for (const path of ["/nope", "/api/auth/agent/list", "/api/auth/sign-in/email", "/api/auth/host/create"]) {
      expect((await get(path, envWithApi(200))).status, path).toBe(404);
    }
  });
});
