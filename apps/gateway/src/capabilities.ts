// Makes each capability's one call on the private API (packages/contracts/openapi.yaml), as its
// definition in @runlater/contracts binds it, validating arguments that end up in a path.

import { capabilities, LANE_PATTERN, STATUSES } from "@runlater/contracts/capabilities";
import type { ApiBinding, CapabilityDefinition } from "@runlater/contracts/capabilities";
import type { ApiClient, Scope } from "./api";

export { capabilities, capabilityNames } from "@runlater/contracts/capabilities";

const LANE_RULE = "1-64 of A-Z a-z 0-9 _ - . :";

/** Raised for bad arguments before anything reaches the API. */
export class ArgumentError extends Error {
  constructor(
    message: string,
    readonly field?: string,
  ) {
    super(message);
  }
}

type Args = Record<string, unknown>;

function only(args: Args, allowed: string[]): void {
  for (const key of Object.keys(args)) {
    if (!allowed.includes(key)) throw new ArgumentError(`unknown argument ${JSON.stringify(key)}`, key);
  }
}

function listQuery(args: Args): string {
  only(args, ["status", "queue", "lane", "cursor", "limit"]);
  if (args.queue != null && args.lane != null) throw new ArgumentError("pass queue or lane, not both", "queue");
  const q = new URLSearchParams();
  if (args.status !== undefined) {
    if (typeof args.status !== "string" || !STATUSES.includes(args.status)) throw new ArgumentError("unknown status", "status");
    q.set("status", args.status);
  }
  const field = args.queue != null ? "queue" : "lane";
  const value = args[field];
  if (value != null) {
    if (typeof value !== "string" || !new RegExp(LANE_PATTERN).test(value)) throw new ArgumentError(`invalid ${field}: ${LANE_RULE}`, field);
    q.set("lane", value);
  }
  if (args.cursor !== undefined) {
    if (typeof args.cursor !== "string" || !/^[0-9]{1,16}$/.test(args.cursor)) throw new ArgumentError("invalid cursor", "cursor");
    q.set("cursor", args.cursor);
  }
  if (args.limit !== undefined) {
    if (!Number.isInteger(args.limit) || (args.limit as number) < 1 || (args.limit as number) > 100) {
      throw new ArgumentError("limit must be 1-100", "limit");
    }
    q.set("limit", String(args.limit));
  }
  const s = q.toString();
  return s ? `?${s}` : "";
}

type Call = { method: "GET" | "POST"; path: string; scope: Scope; body?: unknown };

const definitions = new Map(capabilities.map((c) => [c.name, c]));

/** An argument that becomes part of a path, checked against its input schema's pattern. */
function pathArg(capability: CapabilityDefinition, field: string, value: unknown): string {
  const pattern = capability.input.properties[field]?.pattern;
  if (typeof value !== "string" || typeof pattern !== "string" || !new RegExp(pattern).test(value)) {
    const what = field === "task_id" ? "task id" : `${capability.name.split("_").slice(1).join(" ")} ${field}`;
    throw new ArgumentError(`${field} is not a valid ${what}`, field);
  }
  return value;
}

/** Capabilities whose arguments the API's request doesn't take as they are. */
const prepare: Record<string, (api: ApiBinding, args: Args) => Call> = {
  queue_http(api, args) {
    // A queue is a lane to the API, which validates the rest of the request.
    const { run_at_ms, delay, queue, ...request } = args;
    for (const [field, value] of Object.entries({ run_at_ms, delay })) {
      if (value != null) throw new ArgumentError("queue_http sends as soon as possible; use schedule_http for a later time", field);
    }
    if ("lane" in request) throw new ArgumentError("queue_http takes queue, not lane", "lane");
    only(request, ["url", "method", "headers", "body", "callback_url", "capture", "idempotency_key", "max_attempts"]);
    const call: Call = { method: api.method, path: api.path, scope: api.scope, body: request };
    if (queue == null) return call;
    if (typeof queue !== "string" || !new RegExp(LANE_PATTERN).test(queue)) throw new ArgumentError(`invalid queue: ${LANE_RULE}`, "queue");
    call.body = { ...request, lane: queue };
    return call;
  },
  list_tasks(api, args) {
    return { method: api.method, path: `${api.path}${listQuery(args)}`, scope: api.scope };
  },
};

/**
 * The API call a capability makes, from its definition's binding. The API validates request
 * bodies fully and names the failing field; arguments that end up in a path are checked here.
 * Project allocation and get_payment_link are handled by the caller.
 */
export function route(capability: string, args: Args): Call {
  const definition = definitions.get(capability);
  const api = definition?.api;
  if (!definition || !api) throw new ArgumentError(`unknown capability ${capability}`);
  const custom = prepare[capability];
  if (custom) return custom(api, args);
  const fields = [...api.path.matchAll(/\{(\w+)\}/g)].map((m) => m[1]!);
  if (api.body !== "args" && api.body !== "rest") only(args, fields);
  let path = api.path;
  for (const field of fields) {
    const value = pathArg(definition, field, args[field]);
    path = path.replace(`{${field}}`, () => value);
  }
  const call: Call = { method: api.method, path, scope: api.scope };
  if (api.body === "args") call.body = args;
  if (api.body === "rest") call.body = Object.fromEntries(Object.entries(args).filter(([k]) => !fields.includes(k)));
  if (api.body === "empty") call.body = {};
  return call;
}

export async function execute(client: ApiClient, capability: string, args: Args): Promise<unknown> {
  const r = route(capability, args);
  return client.call(r.method, r.path, r.scope, r.body);
}
