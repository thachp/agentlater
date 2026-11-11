// Agent Auth host: Better Auth with the agent-auth plugin, identities in D1.
// Agents are autonomous: discovery, registration and execution need no human account.

import { agentAuth, agentError, type Capability } from "@better-auth/agent-auth";
import { betterAuth, type BetterAuthOptions, type BetterAuthPlugin } from "better-auth";
import { createAuthMiddleware } from "better-auth/api";
import { a2aUrl, Caller, type Refusal } from "./a2a";
import { ApiError } from "./api";
import { ArgumentError, capabilities, capabilityNames } from "./capabilities";
import { runCapability, type CapabilityEnv } from "./projects";
import type { AuthStore } from "./store";
import { providerNameFor } from "./hosts";

export const BASE_PATH = "/api/auth";
const WEEK_SECS = 7 * 24 * 3600;
/** Plugin JWTs live 60 s plus 30 s clock skew; replay claims must outlast them. */
const JTI_TTL_SECS = 90;

/** The capability that lets an agent call the A2A interface. Not a tool: its location is /a2a, and nothing executes it. */
const A2A_CAPABILITY = "a2a";
/** What a host may grant its agents: every tool, and a2a. */
export const hostCapabilityNames = [...capabilityNames, A2A_CAPABILITY];

export interface AuthEnv extends CapabilityEnv {
  AUTH_STORE: DurableObjectNamespace<AuthStore>;
  BETTER_AUTH_SECRET: string;
}

type OnExecute = NonNullable<Parameters<typeof agentAuth>[0]>["onExecute"];

/** What the gateway reads of the session the plugin builds from a verified agent JWT. */
interface AgentSession {
  agent: { id: string; hostId: string | null; capabilityGrants: { capability: string; status: string }[] };
  host: { status: string } | null;
}

/**
 * The plugin revokes a host's agents one by one after marking the host revoked; if that is
 * interrupted, surviving agents must still be refused. The plugin reads the host for every agent
 * JWT but does not look at its status.
 */
const hostActive = (session: Pick<AgentSession, "host">) => session.host?.status === "active";
const HOST_REVOKED = { code: "host_revoked", message: "this agent's host is not active" };

/** The claims of a request's bearer JWT, read without verifying it. Only for a token the plugin verifies or has verified. */
function bearerClaims(headers: Headers): Record<string, unknown> | null {
  const token = headers.get("Authorization")?.match(/^Bearer\s+(\S+)$/i)?.[1];
  const payload = token?.split(".")[1];
  if (!payload) return null;
  try {
    const claims: unknown = JSON.parse(atob(payload.replace(/-/g, "+").replace(/_/g, "/")));
    return typeof claims === "object" && claims !== null ? (claims as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}

const authStore = (env: AuthEnv) => env.AUTH_STORE.get(env.AUTH_STORE.idFromName("auth-store"));

/**
 * Whether this is the first time a request's bearer JWT is presented, on any route. The plugin
 * has a replay check of its own, but a separate check-then-store that two copies of a request
 * sent at once both pass; the claim here is atomic. A token without a jti passes: the plugin
 * refuses it.
 */
export async function firstUse(env: AuthEnv, request: Request): Promise<boolean> {
  const claims = bearerClaims(request.headers);
  if (typeof claims?.jti !== "string" || claims.jti.length === 0 || claims.jti.length > 128) return true;
  return authStore(env).claim(`replay:${String(claims.iss ?? "")}:${String(claims.sub ?? "")}:${claims.jti}`, JTI_TTL_SECS);
}

/**
 * A token signed for /a2a is good there only. The plugin accepts it on its own routes too, since
 * the a2a capability's location is an audience it allows everywhere, and its execute route then
 * runs any capability the host may grant, whatever the token names. So this hook, which runs after
 * the plugin's and acts only on a token the plugin has verified, refuses one whose audience is, or
 * includes, /a2a. It never runs on /agent/session, which is how /a2a itself asks: there the
 * plugin's hook answers.
 */
const a2aTokensStayOnA2a = (audience: string): BetterAuthPlugin => ({
  id: "a2a-audience",
  hooks: {
    before: [
      {
        matcher: () => true,
        handler: createAuthMiddleware(async (ctx) => {
          const verified = ctx.context as { agentSession?: unknown; hostSession?: unknown };
          if (!ctx.headers || (!verified.agentSession && !verified.hostSession)) return;
          if ([bearerClaims(ctx.headers)?.aud].flat().includes(audience)) {
            throw agentError(401 as never, { code: "invalid_jwt", message: "a token signed for /a2a is accepted only there" });
          }
        }),
      },
    ],
  },
});

/** Options shared by the Worker and the schema generator, which has no env. */
export function authOptions(input: {
  baseURL: string;
  /** Agent Auth's slug. Defaults to agentlater; see `createAuth`. */
  providerName?: string;
  database: BetterAuthOptions["database"];
  secret: string;
  secondaryStorage?: BetterAuthOptions["secondaryStorage"];
  onExecute?: OnExecute;
  background?: (p: Promise<unknown>) => void;
}): BetterAuthOptions {
  // The plugin accepts a capability's location as a token's audience when the token names that capability alone.
  const a2aAudience = a2aUrl({ PUBLIC_URL: input.baseURL });
  const a2a: Capability = {
    name: A2A_CAPABILITY,
    description:
      "Call AgentLater's A2A interface (see /.well-known/agent-card.json). Not executed like the other capabilities: sign an agent JWT " +
      "whose aud is this capability's location and whose capabilities are [\"a2a\"], and send it as the bearer token of each A2A request.",
    location: a2aAudience,
    input: { type: "object", additionalProperties: false, properties: {} },
  };
  return {
    baseURL: input.baseURL,
    basePath: BASE_PATH,
    database: input.database,
    secret: input.secret,
    secondaryStorage: input.secondaryStorage,
    rateLimit: { enabled: true, storage: input.secondaryStorage ? "secondary-storage" : "memory" },
    logger: { disabled: true },
    advanced: input.background ? { backgroundTasks: { handler: input.background } } : undefined,
    plugins: [
      // The plugin pins its own better-call, so its endpoint types don't unify with
      // Better Auth's; the runtime contract is the same.
      agentAuth({
        // A slug: directories such as agent-auth.directory reject anything else.
        providerName: input.providerName ?? "agentlater",
        providerDescription: "Make an HTTP request later, and check whether it happened.",
        modes: ["autonomous"],
        allowedKeyAlgorithms: ["Ed25519"],
        allowDynamicHostRegistration: true,
        capabilities: [...capabilities.map((c): Capability => ({ ...c })), a2a],
        defaultHostCapabilities: hostCapabilityNames,
        validateCapabilities: (requested) => requested.every((c) => hostCapabilityNames.includes(c)),
        jwtMaxAge: 60,
        agentSessionTTL: 3600,
        agentMaxLifetime: WEEK_SECS,
        absoluteLifetime: WEEK_SECS,
        resolveGrantTTL: () => WEEK_SECS,
        jtiCacheStorage: input.secondaryStorage ? "secondary-storage" : "memory",
        jwksCacheStorage: input.secondaryStorage ? "secondary-storage" : "memory",
        // Autonomous agents have no human account; attribute them to their host.
        resolveAutonomousUser: ({ hostId }) => ({
          id: `agent-host:${hostId}`,
          name: "Autonomous AgentLater agent",
          email: `${hostId}@agents.invalid`,
        }),
        onExecute: input.onExecute,
      }) as unknown as BetterAuthPlugin,
      // After agentAuth, so that its hook runs on a token the plugin has already verified.
      a2aTokensStayOnA2a(a2aAudience),
    ],
  };
}

function storage(env: AuthEnv): NonNullable<BetterAuthOptions["secondaryStorage"]> {
  const store = authStore(env);
  return {
    get: (key) => store.get(key),
    getAndDelete: (key) => store.getAndDelete(key),
    increment: (key, ttl) => store.increment(key, ttl),
    set: (key, value, ttl) => store.set(key, typeof value === "string" ? value : JSON.stringify(value), ttl),
    delete: (key) => store.delete(key),
  };
}

interface AgentAuthApi {
  getAgentConfiguration(): Promise<{ endpoints?: Record<string, string> } & Record<string, unknown>>;
}

/**
 * The capabilities an agent holds outright: its active, unexpired grants that carry no
 * constraints. Read here because a token for /a2a names only the a2a capability, so the plugin's
 * session lists no other; a grant with constraints is left out because only the plugin's execute
 * route checks them.
 */
async function grantsOf(db: D1Database, agentId: string, now = Date.now()): Promise<Set<string>> {
  const { results } = await db
    .prepare('SELECT capability, "expiresAt" FROM "agentCapabilityGrant" WHERE "agentId" = ? AND status = ? AND constraints IS NULL')
    .bind(agentId, "active")
    .all<{ capability: string; expiresAt: string | number | null }>();
  return new Set(results.filter((g) => g.expiresAt === null || new Date(g.expiresAt).getTime() > now).map((g) => g.capability));
}

export function createAuth(env: AuthEnv, ctx: ExecutionContext, signingKey: CryptoKey) {
  const onExecute: OnExecute = async ({ capability, arguments: args = {}, agentSession }) => {
    const hostId = agentSession.agent.hostId;
    try {
      if (!hostActive(agentSession)) throw new ApiError(403, { error: HOST_REVOKED });
      if (capability === A2A_CAPABILITY) {
        throw new ApiError(400, { error: { code: "invalid_request", message: `a2a is not executed; it lets an agent call ${a2aUrl(env)}` } });
      }
      return await runCapability(env, signingKey, hostId, capability, args);
    } catch (e) {
      if (e instanceof ApiError) {
        const { code, message, field } = e.body.error;
        throw agentError(e.status as never, { code, message }, undefined, undefined, field ? { field } : undefined);
      }
      if (e instanceof ArgumentError) {
        throw agentError(400 as never, { code: "invalid_request", message: e.message }, undefined, undefined, {
          field: e.field,
        });
      }
      throw e;
    }
  };
  const auth = betterAuth(
    authOptions({
      baseURL: env.PUBLIC_URL,
      providerName: providerNameFor(env.PUBLIC_URL),
      database: env.DB,
      secret: env.BETTER_AUTH_SECRET,
      secondaryStorage: storage(env),
      onExecute,
      background: (p) => ctx.waitUntil(p),
    }),
  );

  /**
   * Who is calling /a2a. The plugin verifies the token: GET /agent/session runs the same check as
   * every Agent Auth route (signature, typ, age and expiry, replay, agent status) and answers
   * with the agent's session. Left to do here, on a token the plugin has accepted: it was signed
   * for /a2a and nothing else, it is used once, its host is active, and the agent holds a2a.
   */
  async function a2aCaller(request: Request): Promise<Caller | Refusal> {
    const invalid = (message: string): Refusal => ({ status: 401, code: "invalid_jwt", message });
    const headers = new Headers({ Authorization: request.headers.get("Authorization") ?? "" });
    // Better Auth's rate limit is per address, which it reads from this header.
    const ip = request.headers.get("CF-Connecting-IP");
    if (ip) headers.set("X-Forwarded-For", ip);
    const response = await auth.handler(new Request(`${env.PUBLIC_URL}${BASE_PATH}/agent/session`, { headers }));
    if (response.status === 429) {
      return { status: 429, code: "rate_limited", message: "too many requests; try again shortly", retryAfter: response.headers.get("X-Retry-After") ?? "60" };
    }
    if (response.status >= 500) return { status: 503, code: "unavailable", message: "the token could not be verified; try again" };
    const session = (await response.json().catch(() => null)) as (Partial<AgentSession> & { error?: unknown; message?: unknown }) | null;
    if (!response.ok) {
      // The plugin's own refusal: 403 for a revoked agent, 401 for anything wrong with the token.
      return {
        status: response.status === 403 ? 403 : 401,
        code: typeof session?.error === "string" ? session.error : "invalid_jwt",
        message: typeof session?.message === "string" ? session.message : "this token is not accepted",
      };
    }
    // No session: not a JWT at all, or a host's JWT, which the plugin verifies but which is no agent's.
    const { agent, host = null } = session ?? {};
    if (!agent?.hostId) return invalid("send an Agent Auth agent JWT");
    // The plugin has accepted the signature over these claims, with any audience it allows.
    const audience = a2aUrl(env);
    if (bearerClaims(request.headers)?.aud !== audience) return invalid(`this token was not signed for /a2a: its aud must be ${audience}`);
    if (!(await firstUse(env, request))) return invalid("this token has already been used");
    if (!hostActive({ host })) return { status: 403, ...HOST_REVOKED };
    // The session lists the agent's grants among those the token names, so this is both.
    if (!agent.capabilityGrants.some((g) => g.capability === A2A_CAPABILITY && g.status === "active")) {
      return { status: 403, code: "capability_not_granted", message: "this agent does not hold the a2a capability; request it, or register an agent with it" };
    }
    return new Caller(agent.hostId, await grantsOf(env.DB, agent.id));
  }

  return { handler: auth.handler, api: auth.api as unknown as AgentAuthApi, a2aCaller };
}
