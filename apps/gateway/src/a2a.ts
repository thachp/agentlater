// The A2A interface (https://a2a-protocol.org/v1.0.1/specification/): a public Agent Card and a
// JSON-RPC endpoint at /a2a, both served only when A2A_ENABLED is "true".

import { A2A_VERSION_HEADER, AgentCard } from "@a2a-js/sdk";
import { A2A_ERROR_CODE, PushNotificationNotSupportedError, UnsupportedOperationError } from "@a2a-js/sdk/errors";
import { JsonRpcTransportHandler, ServerCallContext, validateVersion, type A2ARequestHandler } from "@a2a-js/sdk/server";
import { readLimited } from "./body";
import { SITE } from "./discovery";

export interface A2aEnv {
  PUBLIC_URL: string;
  /** "true" serves the Agent Card and /a2a; anything else answers 404 for both. */
  A2A_ENABLED?: string;
}

export const a2aEnabled = (env: A2aEnv) => env.A2A_ENABLED === "true";

/** Where an agent registers before it can call /a2a. */
const discoveryUrl = (env: A2aEnv) => `${env.PUBLIC_URL}/.well-known/agent-configuration`;

/** The card for the host that was asked. Its skills list only what /a2a will do. */
function card(env: A2aEnv): AgentCard {
  const endpoint = `${env.PUBLIC_URL}/a2a`;
  return AgentCard.fromJSON({
    name: "AgentLater",
    description: "Makes an HTTP request later for an agent, retries it when the destination fails, and records whether it happened.",
    version: "1.0.0",
    provider: { organization: "AgentLater", url: SITE },
    documentationUrl: `${SITE}/docs`,
    supportedInterfaces: [{ url: endpoint, protocolBinding: "JSONRPC", protocolVersion: "1.0" }],
    capabilities: {
      streaming: false,
      pushNotifications: false,
      extendedAgentCard: false,
      extensions: [
        {
          uri: `${SITE}/a2a/extensions/agent-auth/v1`,
          description: "How to get the bearer token: register with Agent Auth at `discovery`, then sign an agent JWT for `audience` that names `capability`.",
          required: false,
          params: { discovery: discoveryUrl(env), audience: endpoint, capability: "a2a" },
        },
      ],
    },
    securitySchemes: {
      agentAuth: {
        httpAuthSecurityScheme: {
          scheme: "Bearer",
          bearerFormat: "JWT",
          description:
            "An Agent Auth agent JWT (typ agent+jwt, Ed25519) whose aud is this interface's URL and whose capabilities are [\"a2a\"]. " +
            "A token lives at most 60 seconds and is accepted once, so sign a new one for every request.",
        },
      },
    },
    securityRequirements: [{ schemes: { agentAuth: { list: [] } } }],
    defaultInputModes: ["application/json"],
    defaultOutputModes: ["application/json"],
    skills: [
      {
        id: "deliver-http",
        name: "Deliver an HTTP request",
        description:
          "Send one JSON data part that describes an HTTP request (url, method, headers, body) and when to send it (run_at_ms or delay; " +
          "neither means now). The reply is a task, returned at once. AgentLater sends the request at that time and retries when the " +
          "destination fails; the task completes once the destination answers 2xx and fails if it never does. Read it with GetTask or " +
          "ListTasks, and cancel it with CancelTask before it is sent.",
        tags: ["http", "webhook", "schedule", "retry"],
        examples: [JSON.stringify({ url: "https://example.com/hook", method: "POST", body: '{"order":42}', delay: "15m", idempotency_key: "order-42" })],
      },
    ],
  });
}

/** The Agent Card as served at /.well-known/agent-card.json. */
export const agentCard = (env: A2aEnv) => AgentCard.toJSON(card(env));

/** 401 with the Agent Auth challenge (its specification, 5.14), so a caller can find where to register. */
const unauthorized = (env: A2aEnv, code: string, message: string) =>
  Response.json(
    { error: code, message },
    { status: 401, headers: { "Cache-Control": "no-store", "WWW-Authenticate": `AgentAuth discovery="${discoveryUrl(env)}"` } },
  );

const unsupported = (what: string) => () => {
  throw new UnsupportedOperationError(`${what} is not supported`);
};
const notYet = unsupported("this operation");
const noStreaming = unsupported("streaming");
const noPush = () => {
  throw new PushNotificationNotSupportedError();
};

/** What each A2A operation does. Nothing runs yet: every operation answers with its standard error. */
const operations: A2ARequestHandler = {
  // Part of the SDK's interface, but its JSON-RPC handler never asks; agentCard() serves the card.
  getAgentCard: notYet,
  getAuthenticatedExtendedAgentCard: unsupported("the extended Agent Card"),
  sendMessage: notYet,
  getTask: notYet,
  listTasks: notYet,
  cancelTask: notYet,
  sendMessageStream: noStreaming,
  resubscribe: noStreaming,
  createTaskPushNotificationConfig: noPush,
  getTaskPushNotificationConfig: noPush,
  listTaskPushNotificationConfigs: noPush,
  deleteTaskPushNotificationConfig: noPush,
};
/** The SDK's dispatcher: method names, parameter and result codecs, and error codes. */
const transport = new JsonRpcTransportHandler(operations);

/** The same limit as the gateway's other JSON endpoints. */
const MAX_BODY_BYTES = 192 * 1024;

/** A single JSON-RPC 2.0 request; batches are not part of the A2A binding. */
const isRpcRequest = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value) && "jsonrpc" in value && value.jsonrpc === "2.0" && "method" in value && typeof value.method === "string";

/** A JSON-RPC answer. Protocol errors travel in the body, so the HTTP status is 200. */
const rpcResponse = (body: unknown) => Response.json(body, { headers: { "Cache-Control": "no-store" } });
const rpcError = (error: unknown) => rpcResponse({ jsonrpc: "2.0", id: null, error });

/** Says whether a request's bearer credential is good. */
type Verify = (request: Request) => Promise<boolean>;
/** Nothing verifies agent JWTs yet, so no credential is good and /a2a runs nothing for anyone. */
const acceptNone: Verify = async () => false;

/**
 * Answers a request to /a2a: the protocol version first (a header, so nothing is read for a caller
 * that speaks another version), then credentials, refused with HTTP 401, and only then JSON-RPC.
 */
export async function handleA2a(request: Request, env: A2aEnv, verify: Verify = acceptNone): Promise<Response> {
  if (request.method !== "POST") {
    return Response.json({ error: "method_not_allowed", message: "POST a JSON-RPC request" }, { status: 405, headers: { Allow: "POST" } });
  }

  // No version means 0.3 (specification 3.6), which the card does not offer.
  const version = request.headers.get(A2A_VERSION_HEADER) ?? new URL(request.url).searchParams.get(A2A_VERSION_HEADER) ?? undefined;
  const context = new ServerCallContext({ requestedVersion: version });
  try {
    validateVersion(context.requestedVersion, card(env), "JSONRPC");
  } catch (e) {
    return rpcError(JsonRpcTransportHandler.mapToJSONRPCError(e));
  }

  if (!/^Bearer\s+\S+$/i.test(request.headers.get("Authorization") ?? "")) {
    return unauthorized(env, "authentication_required", "send an Agent Auth agent JWT as a Bearer token");
  }
  if (!(await verify(request))) return unauthorized(env, "invalid_jwt", "this token is not accepted on /a2a");

  const body = await readLimited(request, MAX_BODY_BYTES);
  if (body === null) return Response.json({ error: "too_large", message: "request body is too large" }, { status: 413 });
  let rpc: unknown;
  try {
    rpc = JSON.parse(new TextDecoder().decode(body));
  } catch {
    return rpcError({ code: A2A_ERROR_CODE.PARSE_ERROR, message: "Invalid JSON payload" });
  }
  // The SDK's handler answers both of these with -32602; the specification (9.5) gives each its own code.
  if (!isRpcRequest(rpc)) return rpcError({ code: A2A_ERROR_CODE.INVALID_REQUEST, message: "Request payload validation error" });
  // Always one answer, never a stream: both streaming methods throw before a stream exists.
  return rpcResponse(await transport.handle(rpc, context));
}
