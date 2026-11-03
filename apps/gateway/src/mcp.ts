// Hosted MCP server at /mcp. Each OAuth grant (see consent.ts) is one owner with its own
// project; the tools are the capabilities themselves, run as they are for Agent Auth agents.
// Stateless: every POST builds a fresh server, so no session survives between requests.

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { WebStandardStreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/webStandardStreamableHttp.js";
import { CallToolRequestSchema, ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import { CfWorkerJsonSchemaValidator } from "@modelcontextprotocol/sdk/validation/cfworker";
import { ApiError } from "./api";
import type { BillingEnv } from "./billing";
import { ArgumentError, capabilities, capabilityNames } from "./capabilities";
import { runCapability } from "./projects";

/** What a grant carries to every /mcp request (`ctx.props`). */
export interface McpProps {
  /** The project owner, `mcp:<id>`. */
  owner: string;
}

export interface McpEnv extends BillingEnv {
  PUBLIC_URL: string;
  DB: D1Database;
  API: Fetcher;
}

const INSTRUCTIONS =
  "RunLater makes HTTP requests later and records whether they happened. Call create_project once, " +
  "then schedule_http (one request), create_cron (a repeating one) or watch_http (poll until done; on_match sends the next request itself when it matches). " +
  "To learn the result in a later session without a public URL of your own, pass callback_url: \"inbox\" " +
  "(and capture: true to keep the response). At the start of a session, if get_project shows inbox_unread above 0, " +
  "call read_inbox and ack what you handle. If something can start you without a person (a routine or workflow trigger), " +
  "give its URL to set_wake and RunLater starts you when results arrive. Never paste a credential into a request: store it " +
  "with set_secret (bound to the hosts it is for) and use {{secret:NAME}} in a header. Check a single task with get_task. " +
  "When the project runs out of attempts, get_payment_link gives your user a link to buy more.";

/** A schedule_http body can be 64 KiB of string; leave room for JSON-RPC and escaping. */
const MAX_BODY_BYTES = 192 * 1024;

const tools = capabilities.map((c) => ({ name: c.name, description: c.description, inputSchema: c.input }));

function result(value: unknown, isError = false) {
  const structured = value && typeof value === "object" && !Array.isArray(value) ? { structuredContent: value as Record<string, unknown> } : {};
  return { content: [{ type: "text" as const, text: JSON.stringify(value, null, 2) }], ...structured, isError };
}

/** Handles one /mcp request for an authenticated grant. */
export async function handleMcp(request: Request, env: McpEnv, key: CryptoKey, props: McpProps): Promise<Response> {
  // Stateless: no server-initiated stream (GET) and no session to end (DELETE).
  if (request.method !== "POST") {
    return new Response(null, { status: 405, headers: { Allow: "POST" } });
  }
  const server = new Server(
    { name: "runlater", version: "1.0.0" },
    { capabilities: { tools: {} }, instructions: INSTRUCTIONS, jsonSchemaValidator: new CfWorkerJsonSchemaValidator() },
  );
  server.setRequestHandler(ListToolsRequestSchema, async () => ({ tools }));
  server.setRequestHandler(CallToolRequestSchema, async ({ params }) => {
    if (!capabilityNames.includes(params.name)) {
      return result({ error: { code: "unknown_tool", message: `no tool named ${params.name}` } }, true);
    }
    try {
      const value = await runCapability(env, key, props.owner, params.name, params.arguments ?? {});
      return result(value);
    } catch (e) {
      // Agent mistakes go back to the model as tool errors it can read and fix.
      if (e instanceof ApiError) return result(e.body, true);
      if (e instanceof ArgumentError) {
        return result({ error: { code: "invalid_request", message: e.message, ...(e.field ? { field: e.field } : {}) } }, true);
      }
      console.error("tool failed", params.name, e);
      return result({ error: { code: "internal_error", message: "RunLater could not complete this call; try again" } }, true);
    }
  });
  const transport = new WebStandardStreamableHTTPServerTransport({
    sessionIdGenerator: undefined,
    enableJsonResponse: true,
    maxRequestBodySize: MAX_BODY_BYTES,
  });
  await server.connect(transport);
  try {
    return await transport.handleRequest(request);
  } finally {
    await server.close();
  }
}
