import { describe, expect, it } from "vitest";
import { ApiError } from "../src/api";
import { runCapability, TOOL_CALLS_PER_MINUTE } from "../src/projects";

describe("tool calls", () => {
  const db = { prepare: () => ({ bind: () => ({ first: async () => ({ project_id: "prj_1" }) }) }) } as unknown as D1Database;

  it("are refused past the owner's limit, before the API is called", async () => {
    const key = (await crypto.subtle.generateKey("Ed25519", false, ["sign", "verify"])) as CryptoKeyPair;
    const keys: string[] = [];
    let apiCalls = 0;
    const env = {
      DB: db,
      API: { fetch: async () => (apiCalls++, Response.json({ tasks: [] })) } as unknown as Fetcher,
      PUBLIC_URL: "https://gateway.test",
      TOOL_RATE: { limit: async ({ key }: { key: string }) => (keys.push(key), { success: false }) } as unknown as RateLimit,
    };
    const e = await runCapability(env, key.privateKey, "mcp:grant_1", "list_tasks", {}).catch((e: unknown) => e);
    expect(e).toBeInstanceOf(ApiError);
    expect((e as ApiError).status).toBe(429);
    expect((e as ApiError).body.error.code).toBe("rate_limited");
    expect((e as ApiError).body.error.message).toContain(String(TOOL_CALLS_PER_MINUTE));
    expect(keys).toEqual(["mcp:grant_1"]);
    expect(apiCalls).toBe(0);
  });
});

describe("TOOL_RATE in wrangler.jsonc", () => {
  it("allows the number of calls the error message names", async () => {
    const { readFileSync } = await import("node:fs");
    const { dirname, join } = await import("node:path");
    const { fileURLToPath } = await import("node:url");
    const config = readFileSync(join(dirname(fileURLToPath(import.meta.url)), "../wrangler.jsonc"), "utf8");
    const limits = [...config.matchAll(/"name": "TOOL_RATE"[^}]*"limit": (\d+)/g)].map((m) => Number(m[1]));
    expect(limits.length).toBeGreaterThan(0);
    for (const limit of limits) expect(limit).toBe(TOOL_CALLS_PER_MINUTE);
  });
});
