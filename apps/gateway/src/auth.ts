// Agent Auth host: Better Auth with the agent-auth plugin, identities in D1.
// Agents are autonomous: discovery, registration and execution need no human account.

import { agentAuth, agentError, type Capability } from "@better-auth/agent-auth";
import { betterAuth, type BetterAuthOptions, type BetterAuthPlugin } from "better-auth";
import { ApiError } from "./api";
import { ArgumentError, capabilities, capabilityNames } from "./capabilities";
import { runCapability } from "./projects";
import type { AuthStore } from "./store";
import type { BillingEnv } from "./billing";

export const BASE_PATH = "/api/auth";
const WEEK_SECS = 7 * 24 * 3600;

export interface AuthEnv extends BillingEnv {
  DB: D1Database;
  API: Fetcher;
  AUTH_STORE: DurableObjectNamespace<AuthStore>;
  PUBLIC_URL: string;
  BETTER_AUTH_SECRET: string;
}

type OnExecute = NonNullable<Parameters<typeof agentAuth>[0]>["onExecute"];

/** Options shared by the Worker and the schema generator, which has no env. */
export function authOptions(input: {
  baseURL: string;
  database: BetterAuthOptions["database"];
  secret: string;
  secondaryStorage?: BetterAuthOptions["secondaryStorage"];
  onExecute?: OnExecute;
  background?: (p: Promise<unknown>) => void;
}): BetterAuthOptions {
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
        providerName: "runlater",
        providerDescription: "Make an HTTP request later, and check whether it happened.",
        modes: ["autonomous"],
        allowedKeyAlgorithms: ["Ed25519"],
        allowDynamicHostRegistration: true,
        capabilities: capabilities.map((c): Capability => ({ ...c })),
        defaultHostCapabilities: capabilityNames,
        validateCapabilities: (requested) => requested.every((c) => capabilityNames.includes(c)),
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
          name: "Autonomous RunLater agent",
          email: `${hostId}@agents.invalid`,
        }),
        onExecute: input.onExecute,
      }) as unknown as BetterAuthPlugin,
    ],
  };
}

function storage(env: AuthEnv): NonNullable<BetterAuthOptions["secondaryStorage"]> {
  const store = env.AUTH_STORE.get(env.AUTH_STORE.idFromName("auth-store"));
  return {
    get: (key) => store.get(key),
    getAndDelete: (key) => store.getAndDelete(key),
    increment: (key, ttl) => store.increment(key, ttl),
    set: (key, value, ttl) => store.set(key, typeof value === "string" ? value : JSON.stringify(value), ttl),
    delete: (key) => store.delete(key),
  };
}

export interface AgentAuthApi {
  getAgentConfiguration(): Promise<{ endpoints?: Record<string, string> } & Record<string, unknown>>;
}

export function createAuth(env: AuthEnv, ctx: ExecutionContext, signingKey: CryptoKey) {
  const onExecute: OnExecute = async ({ capability, arguments: args = {}, agentSession }) => {
    const hostId = agentSession.agent.hostId;
    try {
      // The plugin revokes a host's agents one by one after marking the host revoked; if
      // that is interrupted, surviving agents must still be refused here.
      const host = await env.DB.prepare('SELECT status FROM "agentHost" WHERE id = ?')
        .bind(hostId)
        .first<{ status: string }>();
      if (host?.status !== "active") {
        throw new ApiError(403, { error: { code: "host_revoked", message: "this agent's host is not active" } });
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
      database: env.DB,
      secret: env.BETTER_AUTH_SECRET,
      secondaryStorage: storage(env),
      onExecute,
      background: (p) => ctx.waitUntil(p),
    }),
  );
  return { handler: auth.handler, api: auth.api as unknown as AgentAuthApi };
}
