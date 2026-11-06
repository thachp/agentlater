// Public entry for agents. Serves Agent Auth discovery and the agent endpoints, the hosted MCP
// server at /mcp behind OAuth, Stripe's payment webhook and MPP purchases, and turns capability executions into signed calls on the
// private Rust API (API binding). Also serves inbound hooks (/h/…), on the hooks host.

import { OAuthProvider, type OAuthHelpers } from "@cloudflare/workers-oauth-provider";
import { importSigningKey } from "./api";
import { BASE_PATH, createAuth, type AuthEnv } from "./auth";
import { handleStripeWebhook, type BillingEnv } from "./billing";
import { llmsTxt, openApi } from "./discovery";
import { readLimited } from "./body";
import { handleAuthorize } from "./consent";
import { handleHook, isHookPath, type HooksEnv } from "./hooks";
import { envForHost, isHooksOnlyHost, type HostEnv } from "./hosts";
import { handleMcp, type McpProps } from "./mcp";
import { handleMpp } from "./mpp";

export { AuthStore } from "./store";

export interface Env extends AuthEnv, BillingEnv, HooksEnv, HostEnv {
  /** Hex-encoded 32-byte Ed25519 seed; its public key is the API's GATEWAY_PUBLIC_KEY. */
  ASSERTION_PRIVATE_KEY: string;
  REGISTER_RATE: RateLimit;
  OAUTH_RATE: RateLimit;
  /** MPP purchases, per IP and per project. */
  MPP_RATE: RateLimit;
  /** OAuth clients, grants and tokens for hosted MCP, managed by workers-oauth-provider. */
  OAUTH_KV: KVNamespace;
  /** Set by OAuthProvider before it calls the handlers below. */
  OAUTH_PROVIDER: OAuthHelpers;
}

const DAY_SECS = 24 * 3600;

/** Agent Auth endpoints an autonomous agent needs; everything else is not served. */
const AGENT_ROUTES = new Set([
  "/agent/register",
  "/agent/status",
  "/agent/revoke",
  "/agent/rotate-key",
  "/agent/reactivate",
  "/agent/request-capability",
  "/capability/list",
  "/capability/describe",
  "/capability/execute",
  "/host/revoke",
  "/host/rotate-key",
]);

/** A schedule_http body can be 64 KiB of string; leave room for JSON escaping. */
const MAX_BODY_BYTES = 192 * 1024;
/** Plugin JWTs live 60 s plus 30 s clock skew; replay claims must outlast them. */
const JTI_TTL_SECS = 90;

const json = (body: unknown, status = 200) =>
  Response.json(body, { status, headers: { "Cache-Control": "no-store" } });
const error = (status: number, code: string, message: string) => json({ error: code, message }, status);

/**
 * The replay key for a bearer JWT, read without verifying it. Better Auth verifies the
 * token afterwards; this only lets the gateway reject a second use atomically, which the
 * plugin's own separate check-then-store cannot do under concurrency.
 */
function replayKey(request: Request): string | null {
  const token = request.headers.get("Authorization")?.match(/^Bearer\s+(\S+)$/i)?.[1];
  const payload = token?.split(".")[1];
  if (!payload) return null;
  try {
    const claims = JSON.parse(atob(payload.replace(/-/g, "+").replace(/_/g, "/"))) as Record<string, unknown>;
    if (typeof claims.jti !== "string" || claims.jti.length === 0 || claims.jti.length > 128) return null;
    return `replay:${String(claims.iss ?? "")}:${String(claims.sub ?? "")}:${claims.jti}`;
  } catch {
    return null;
  }
}

let keyCache: { seed: string; key: Promise<CryptoKey> } | undefined;
function signingKey(env: Env): Promise<CryptoKey> {
  if (keyCache?.seed !== env.ASSERTION_PRIVATE_KEY) {
    keyCache = { seed: env.ASSERTION_PRIVATE_KEY, key: importSigningKey(env.ASSERTION_PRIVATE_KEY) };
  }
  return keyCache.key;
}

/** Everything except /mcp and the OAuth endpoints, which OAuthProvider serves first. */
const gateway = {
  async fetch(request: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
    const url = new URL(request.url);

    if (url.pathname === "/authorize") return handleAuthorize(request, env);

    const mppToken = url.pathname.startsWith("/billing/mpp/") ? url.pathname.slice("/billing/mpp/".length) : null;
    if (url.pathname === "/billing/webhook" || mppToken !== null) {
      let key: CryptoKey;
      try {
        key = await signingKey(env);
      } catch {
        return error(500, "misconfigured", "assertion signing key is not configured");
      }
      return mppToken !== null ? handleMpp(request, env, key, mppToken) : handleStripeWebhook(request, env, key);
    }

    if (request.method === "GET" && url.pathname === "/openapi.json") {
      return Response.json(openApi(env), { headers: { "Cache-Control": "public, max-age=300", "Access-Control-Allow-Origin": "*" } });
    }
    if (request.method === "GET" && url.pathname === "/llms.txt") {
      return new Response(llmsTxt(env), {
        headers: { "Content-Type": "text/plain; charset=utf-8", "Cache-Control": "public, max-age=300", "Access-Control-Allow-Origin": "*" },
      });
    }

    if (request.method === "GET" && url.pathname === "/health") {
      const api = await env.API.fetch("https://api.internal/health");
      return json(
        { service: "runlater-gateway", status: "ok", api: api.ok ? "ok" : "unavailable" },
        api.ok ? 200 : 503,
      );
    }

    const isDiscovery = url.pathname === "/.well-known/agent-configuration";
    const route = url.pathname.startsWith(BASE_PATH + "/") ? url.pathname.slice(BASE_PATH.length) : null;
    if (!isDiscovery && !(route && AGENT_ROUTES.has(route))) return error(404, "not_found", "no such route");

    const tooLarge = () => error(413, "too_large", "request body is too large");
    if (Number(request.headers.get("Content-Length") ?? 0) > MAX_BODY_BYTES) return tooLarge();
    const body = await readLimited(request, MAX_BODY_BYTES);
    if (body === null) return tooLarge();

    if (route === "/agent/register") {
      const ip = request.headers.get("CF-Connecting-IP") ?? "unknown";
      const { success } = await env.REGISTER_RATE.limit({ key: ip });
      if (!success) return error(429, "rate_limited", "too many registrations; try again shortly");
    }

    let key: CryptoKey;
    try {
      key = await signingKey(env);
    } catch {
      return error(500, "misconfigured", "assertion signing key is not configured");
    }
    const auth = createAuth(env, ctx, key);

    if (isDiscovery) {
      const config = await auth.api.getAgentConfiguration();
      // Advertise only the endpoints served above.
      if (config.endpoints) {
        config.endpoints = Object.fromEntries(
          Object.entries(config.endpoints).filter(([, u]) => AGENT_ROUTES.has(new URL(u).pathname.slice(BASE_PATH.length))),
        );
      }
      return json(config);
    }

    const replay = replayKey(request);
    if (replay) {
      const store = env.AUTH_STORE.get(env.AUTH_STORE.idFromName("auth-store"));
      if (!(await store.claim(replay, JTI_TTL_SECS))) {
        return error(401, "invalid_jwt", "this token has already been used");
      }
    }
    const buffered = new Request(request.url, {
      method: request.method,
      headers: request.headers,
      body: request.method === "GET" || request.method === "HEAD" ? null : body,
    });
    return auth.handler(buffered);
  },
} satisfies ExportedHandler<Env>;

const mcp = {
  async fetch(request: Request, env: Env, ctx: ExecutionContext & { props: McpProps }): Promise<Response> {
    let key: CryptoKey;
    try {
      key = await signingKey(env);
    } catch {
      return error(500, "misconfigured", "assertion signing key is not configured");
    }
    return handleMcp(request, env, key, ctx.props);
  },
};

/** One provider per public URL: the protected resource's identifier is that URL plus /mcp. */
const providers = new Map<string, OAuthProvider<Env>>();
function provider(env: Env): OAuthProvider<Env> {
  let p = providers.get(env.PUBLIC_URL);
  if (!p) {
    p = new OAuthProvider<Env>({
      apiRoute: "/mcp",
      apiHandler: mcp as never,
      defaultHandler: gateway,
      authorizeEndpoint: "/authorize",
      tokenEndpoint: "/oauth/token",
      clientRegistrationEndpoint: "/oauth/register",
      accessTokenTTL: 3600,
      // The grant is the connection's only identity; keep it while the client keeps using it.
      refreshTokenTTL: 30 * DAY_SECS,
      refreshTokenIdleTTL: 30 * DAY_SECS,
      resourceMetadata: { resource: `${env.PUBLIC_URL}/mcp`, resource_name: "RunLater" },
    });
    providers.set(env.PUBLIC_URL, p);
  }
  return p;
}

/** Unauthenticated OAuth calls that write to KV: registering a client, and each consent page. */
const OAUTH_LIMITED = new Set(["/oauth/register", "/authorize"]);

export default {
  async fetch(request: Request, baseEnv: Env, ctx: ExecutionContext): Promise<Response> {
    const url = new URL(request.url);
    const path = url.pathname;
    const env = envForHost(baseEnv, url.host);
    // Hooks are for the open internet, so they bypass OAuth and Agent Auth, and the hooks hosts
    // serve nothing else.
    if (isHookPath(path) || isHooksOnlyHost(env, url.host)) {
      if (!isHookPath(path)) return error(404, "not_found", "no such route");
      let key: CryptoKey;
      try {
        key = await signingKey(env);
      } catch {
        return error(500, "misconfigured", "assertion signing key is not configured");
      }
      return handleHook(request, env, key);
    }
    if (OAUTH_LIMITED.has(path) && request.method !== "OPTIONS") {
      const ip = request.headers.get("CF-Connecting-IP") ?? "unknown";
      if (!(await env.OAUTH_RATE.limit({ key: `${path}:${ip}` })).success) {
        return error(429, "rate_limited", "too many requests; try again shortly");
      }
    }
    return provider(env).fetch(request, env, ctx);
  },
} satisfies ExportedHandler<Env>;
