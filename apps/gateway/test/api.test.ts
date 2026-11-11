import { describe, expect, it } from "vitest";
import { ApiClient, ApiError, importSigningKey, signAssertion } from "../src/api";

const SEED = "07".repeat(32);

async function publicKeyFor(seedHex: string): Promise<CryptoKey> {
  const pkcs8 = Uint8Array.from(
    ("302e020100300506032b657004220420" + seedHex).match(/../g)!.map((b) => parseInt(b, 16)),
  );
  const priv = await crypto.subtle.importKey("pkcs8", pkcs8, { name: "Ed25519" }, true, ["sign"]);
  const jwk = (await crypto.subtle.exportKey("jwk", priv)) as JsonWebKey;
  return crypto.subtle.importKey("jwk", { kty: "OKP", crv: "Ed25519", x: jwk.x }, { name: "Ed25519" }, true, [
    "verify",
  ]);
}

const decode = (s: string) => JSON.parse(atob(s.replace(/-/g, "+").replace(/_/g, "/")));
const bytes = (s: string) =>
  Uint8Array.from(atob(s.replace(/-/g, "+").replace(/_/g, "/")), (c) => c.charCodeAt(0));

describe("assertions", () => {
  it("signs claims the API accepts", async () => {
    const key = await importSigningKey(SEED);
    const token = await signAssertion(key, "prj_1", ["read", "schedule"], { nowSecs: 1_000 });
    const [payload, sig] = token.split(".");
    expect(decode(payload!)).toEqual({
      aud: "runlater-api",
      sub: "prj_1",
      scope: "read schedule",
      iat: 1_000,
      exp: 1_060,
    });
    const ok = await crypto.subtle.verify(
      "Ed25519",
      await publicKeyFor(SEED),
      bytes(sig!),
      new TextEncoder().encode(payload),
    );
    expect(ok).toBe(true);
  });

  it("produces the exact token the Rust API's tests verify", async () => {
    // Pinned in apps/api/src/assertion.rs (accepts_a_token_signed_by_the_gateway).
    const token = await signAssertion(await importSigningKey(SEED), "prj_1", ["read", "schedule"], { nowSecs: 1_767_225_600 });
    expect(token).toBe(
      "eyJhdWQiOiJydW5sYXRlci1hcGkiLCJzdWIiOiJwcmpfMSIsInNjb3BlIjoicmVhZCBzY2hlZHVsZSIsImlhdCI6MTc2NzIyNTYwMCwiZXhwIjoxNzY3MjI1NjYwfQ.M-yCfOuEo_oLgifHXheOnecRoRyAjPf8bOIvfB40Nn5gBX1nYyubLruWjKOqCQSJXjcv81ogOT6yvv0MbHNECQ",
    );
  });

  it("rejects malformed seeds", async () => {
    await expect(importSigningKey("abcd")).rejects.toThrow();
    await expect(importSigningKey("zz".repeat(32))).rejects.toThrow();
  });
});

describe("ApiClient", () => {
  const fetcher = (status: number, body: unknown, seen: Request[] = []) =>
    ({
      fetch: async (input: RequestInfo, init?: RequestInit) => {
        seen.push(new Request(input, init));
        return Response.json(body, { status });
      },
    }) as unknown as Fetcher;

  it("sends a scoped assertion and returns JSON", async () => {
    const seen: Request[] = [];
    const client = new ApiClient(fetcher(200, { id: "t" }, seen), await importSigningKey(SEED), "prj_1");
    expect(await client.call("POST", "/v1/tasks", "schedule", { url: "x" })).toEqual({ id: "t" });
    const token = seen[0]!.headers.get("RunLater-Assertion")!;
    expect(decode(token.split(".")[0]!).scope).toBe("schedule");
    expect(await seen[0]!.json()).toEqual({ url: "x" });
  });

  it("says a call came through A2A only when it is made for A2A", async () => {
    const seen: Request[] = [];
    const key = await importSigningKey(SEED);
    await new ApiClient(fetcher(200, {}, seen), key, "prj_1").call("GET", "/v1/tasks/t", "read");
    await new ApiClient(fetcher(200, {}, seen), key, "prj_1", "a2a").call("GET", "/v1/tasks/t", "read");
    const claims = seen.map((r) => decode(r.headers.get("RunLater-Assertion")!.split(".")[0]!));
    expect(claims.map((c) => c.via)).toEqual([undefined, "a2a"]);
    expect(Object.keys(claims[0])).toEqual(["aud", "sub", "scope", "iat", "exp"]);
  });

  it("passes API errors through", async () => {
    const body = { error: { code: "invalid_request", message: "bad", field: "url" } };
    const client = new ApiClient(fetcher(400, body), await importSigningKey(SEED), "prj_1");
    const err: any = await client.call("POST", "/v1/tasks", "schedule", {}).catch((e) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect(err.status).toBe(400);
    expect(err.body).toEqual(body);
  });

  it("wraps non-JSON failures", async () => {
    const api = { fetch: async () => new Response("boom", { status: 502 }) } as unknown as Fetcher;
    const err: any = await new ApiClient(api, await importSigningKey(SEED), "p").call("GET", "/x", "read").catch((e) => e);
    expect(err.body.error.code).toBe("api_unavailable");
  });
});
