import { describe, expect, it } from "vitest";
import { envForHost, isHooksOnlyHost, providerNameFor } from "../src/hosts";
import worker, { type Env } from "../src/index";

const hosts = {
  PUBLIC_URL: "https://gateway.old.test",
  HOOKS_URL: "https://hooks.old.test",
  ALT_PUBLIC_URL: "https://gateway.new.test",
  ALT_HOOKS_URL: "https://hooks.new.test",
};
const ctx = { waitUntil() {}, passThroughOnException() {} } as unknown as ExecutionContext;
const env = { ...hosts, API: { fetch: async () => new Response(null, { status: 200 }) } } as unknown as Env;
const get = (url: string) => worker.fetch(new Request(url), env, ctx);

describe("envForHost", () => {
  it("leaves the primary host and unknown hosts alone", () => {
    expect(envForHost(hosts, "gateway.old.test")).toBe(hosts);
    expect(envForHost(hosts, "elsewhere.test")).toBe(hosts);
  });

  it("gives the other domain its own URLs and keeps every binding", () => {
    const binding = { fetch() {} };
    const seen = envForHost({ ...hosts, API: binding }, "gateway.new.test");
    expect(seen.PUBLIC_URL).toBe("https://gateway.new.test");
    expect(seen.HOOKS_URL).toBe("https://hooks.new.test");
    expect(seen.API).toBe(binding);
  });

  it("changes nothing when no other domain is configured", () => {
    const single = { PUBLIC_URL: "https://gateway.old.test" };
    expect(envForHost(single, "gateway.old.test")).toBe(single);
  });
});

describe("isHooksOnlyHost", () => {
  it("is true for both hooks hosts and false for both gateways", () => {
    expect(isHooksOnlyHost(hosts, "hooks.old.test")).toBe(true);
    expect(isHooksOnlyHost(hosts, "hooks.new.test")).toBe(true);
    expect(isHooksOnlyHost(hosts, "gateway.old.test")).toBe(false);
    expect(isHooksOnlyHost(hosts, "gateway.new.test")).toBe(false);
  });

  it("is false when hooks share the gateway's host", () => {
    const shared = { PUBLIC_URL: "https://gateway.old.test", HOOKS_URL: "https://gateway.old.test" };
    expect(isHooksOnlyHost(shared, "gateway.old.test")).toBe(false);
  });
});

describe("providerNameFor", () => {
  it("keeps the old Agent Auth name on the old domain only", () => {
    expect(providerNameFor("https://gateway.runlater.io")).toBe("runlater");
    expect(providerNameFor("https://gateway-staging.runlater.io")).toBe("runlater");
    expect(providerNameFor("https://gateway.agentlater.io")).toBe("agentlater");
    expect(providerNameFor("http://localhost:8787")).toBe("agentlater");
  });
});

describe("gateway on two domains", () => {
  it("names the host that was asked in what it publishes", async () => {
    expect(await (await get("https://gateway.old.test/llms.txt")).text()).toContain("https://gateway.old.test/mcp");
    const other = await (await get("https://gateway.new.test/llms.txt")).text();
    expect(other).toContain("https://gateway.new.test/mcp");
    expect(other).not.toContain("gateway.old.test");
  });

  it("gives each host its own OAuth resource", async () => {
    const resource = async (host: string) =>
      ((await (await get(`https://${host}/.well-known/oauth-protected-resource/mcp`)).json()) as { resource: string }).resource;
    expect(await resource("gateway.old.test")).toBe("https://gateway.old.test/mcp");
    expect(await resource("gateway.new.test")).toBe("https://gateway.new.test/mcp");
  });

  it("serves only hooks on both hooks hosts", async () => {
    for (const host of ["hooks.old.test", "hooks.new.test"]) {
      expect((await get(`https://${host}/health`)).status).toBe(404);
    }
    expect((await get("https://gateway.new.test/health")).status).toBe(200);
  });
});
