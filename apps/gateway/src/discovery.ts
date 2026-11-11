// Public discovery documents. /openapi.json describes the paid MPP endpoint with `x-payment-info`
// so MPP clients and directories can find it (https://mpp.dev); /llms.txt tells a model what
// AgentLater is and how to connect, and names the A2A interface only where it is served. The
// runtime 402 challenge stays authoritative.

import { ATTEMPT_PACK } from "@runlater/contracts/billing";
import type { BillingEnv } from "./billing";
import { mppEnabled } from "./mpp";

export const SITE = "https://agentlater.io";
const attempts = ATTEMPT_PACK.attempts.toLocaleString("en-US");
const price = `$${(ATTEMPT_PACK.priceCents / 100).toFixed(2)}`;

export function openApi(env: BillingEnv & { PUBLIC_URL: string }): Record<string, unknown> {
  const error = { type: "object", properties: { error: { type: "object", properties: { code: { type: "string" }, message: { type: "string" } } } } };
  const paths: Record<string, unknown> = {};
  if (mppEnabled(env)) {
    paths["/billing/mpp/{link}"] = {
      post: {
        operationId: "buyAttemptPack",
        summary: `Buy ${attempts} delivery attempts for ${price}`,
        description:
          `Adds ${attempts} delivery attempts to one AgentLater project, valid 12 months. {link} comes from the get_payment_link ` +
          "capability (its mpp_url) and names the project. Without a Payment credential this answers 402 with a Stripe charge " +
          "challenge (cards via Shared Payment Tokens).",
        parameters: [
          { name: "link", in: "path", required: true, description: "The payment link from get_payment_link's mpp_url.", schema: { type: "string" } },
        ],
        requestBody: { required: false, content: { "application/json": { schema: { type: "object" } } } },
        "x-payment-info": {
          method: "stripe",
          intent: "charge",
          amount: String(ATTEMPT_PACK.priceCents),
          currency: ATTEMPT_PACK.currency,
          description: `${attempts} AgentLater delivery attempts`,
        },
        responses: {
          "200": {
            description: "Paid; the attempts were added. Carries a Payment-Receipt header.",
            content: {
              "application/json": {
                schema: {
                  type: "object",
                  properties: { status: { type: "string" }, attempts_added: { type: "integer" }, payment_id: { type: "string" }, project: { type: "object" } },
                },
              },
            },
          },
          "202": { description: "Paid; the attempts will be added within a few minutes." },
          "402": { description: "Payment required: the WWW-Authenticate header carries the challenge." },
          "404": { description: "The link is invalid or expired.", content: { "application/json": { schema: error } } },
          "429": { description: "Too many payment attempts.", content: { "application/json": { schema: error } } },
        },
      },
    };
  }
  return {
    openapi: "3.1.0",
    info: {
      title: "AgentLater",
      version: "1",
      description: "Make an HTTP request later, and know whether it happened. Agents use AgentLater through MCP or Agent Auth; this document lists its paid HTTP endpoint.",
    },
    servers: [{ url: env.PUBLIC_URL }],
    "x-service-info": {
      categories: ["developer-tools", "scheduling"],
      docs: { homepage: SITE, apiReference: `${SITE}/docs/capabilities`, llms: "/llms.txt" },
    },
    paths,
  };
}

/** `a2a` adds the A2A interface, which is served only where it is switched on. */
export function llmsTxt(env: { PUBLIC_URL: string }, a2a: boolean): string {
  const a2aLine = a2a
    ? `\n- A2A (protocol 1.0, JSON-RPC): Agent Card at ${env.PUBLIC_URL}/.well-known/agent-card.json. Register with Agent Auth first, with the a2a capability; send one JSON data part describing the HTTP request and get a task to poll. One-off deliveries only. Guide: ${SITE}/docs/a2a`
    : "";
  return `# AgentLater

> Make an HTTP request later, and know whether it happened. AgentLater schedules signed HTTPS requests for AI agents, retries them when the destination fails, and records every attempt.

- Hosted MCP server: ${env.PUBLIC_URL}/mcp (Streamable HTTP, OAuth with a one-click consent page, no account).
- Agent Auth: discovery at ${env.PUBLIC_URL}/.well-known/agent-configuration.${a2aLine}
- Capabilities: create_project, schedule_http, queue_http, get_task, list_tasks, cancel_task, redrive_task, create_cron, watch_http, read_inbox, ack_inbox, set_wake, set_secret, create_hook, get_payment_link and more. Reference: ${SITE}/docs/capabilities
- No public URL? schedule_http or queue_http with callback_url "inbox" and capture: true, then read_inbox in your next session: the result, including what the destination answered, waits there. With set_wake, AgentLater calls the URL that starts you as soon as something arrives.
- Credentials: set_secret stores one encrypted and bound to allowed_hosts; use {{secret:NAME}} in a header and AgentLater fills it in only when sending, only to those hosts.
- Pricing: a free trial of 1,000 delivery attempts over 7 days, then ${attempts} attempts for ${price}, valid 12 months. get_payment_link returns a card checkout link for a person and an MPP link (POST, 402) an agent can pay itself.
- Docs: ${SITE}/docs
`;
}
