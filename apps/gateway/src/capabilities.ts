// Maps each capability (defined in @runlater/contracts) to exactly one call on the private
// API (packages/contracts/openapi.yaml), validating arguments that end up in a path.

import { CRON_NAME_PATTERN, LANE_PATTERN, SECRET_NAME_PATTERN, STATUSES } from "@runlater/contracts/capabilities";
import type { ApiClient, Scope } from "./api";

export { capabilities, capabilityNames } from "@runlater/contracts/capabilities";

const TASK_ID_RE = /^task_[0-9a-f]{20}$/;

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

function taskId(args: Args): string {
  only(args, ["task_id"]);
  const id = args.task_id;
  // Validated here because it becomes part of an API path.
  if (typeof id !== "string" || !TASK_ID_RE.test(id)) throw new ArgumentError("task_id is not a valid task id", "task_id");
  return id;
}

function cronName(args: Args): string {
  only(args, ["name"]);
  const name = args.name;
  // Validated here because it becomes part of an API path.
  if (typeof name !== "string" || !new RegExp(CRON_NAME_PATTERN).test(name)) throw new ArgumentError("name is not a valid cron name", "name");
  return name;
}

function hookName(args: Args): string {
  only(args, ["name"]);
  const name = args.name;
  // Validated here because it becomes part of an API path.
  if (typeof name !== "string" || !new RegExp(CRON_NAME_PATTERN).test(name)) throw new ArgumentError("name is not a valid hook name", "name");
  return name;
}

function secretName(args: Args): string {
  only(args, ["name"]);
  const name = args.name;
  // Validated here because it becomes part of an API path.
  if (typeof name !== "string" || !new RegExp(SECRET_NAME_PATTERN).test(name)) throw new ArgumentError("name is not a valid secret name", "name");
  return name;
}

function listQuery(args: Args): string {
  only(args, ["status", "queue", "lane", "cursor", "limit"]);
  if (args.queue !== undefined && args.lane !== undefined) throw new ArgumentError("pass queue or lane, not both", "queue");
  const q = new URLSearchParams();
  if (args.status !== undefined) {
    if (typeof args.status !== "string" || !STATUSES.includes(args.status)) throw new ArgumentError("unknown status", "status");
    q.set("status", args.status);
  }
  const field = args.queue !== undefined ? "queue" : "lane";
  if (args[field] !== undefined) {
    const value = args[field];
    if (typeof value !== "string" || !new RegExp(LANE_PATTERN).test(value)) throw new ArgumentError(`invalid ${field}`, field);
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

/** The API call a capability makes. Project allocation and get_payment_link are handled by the caller. */
export function route(capability: string, args: Args): { method: "GET" | "POST"; path: string; scope: Scope; body?: unknown } {
  switch (capability) {
    case "create_project":
      only(args, []);
      return { method: "POST", path: "/v1/projects", scope: "project" };
    case "get_project":
      only(args, []);
      return { method: "GET", path: "/v1/projects/self", scope: "read" };
    case "schedule_http":
      // The API validates the request fully and names the failing field.
      return { method: "POST", path: "/v1/tasks", scope: "schedule", body: args };
    case "queue_http": {
      // A queue is a lane to the API, which validates the rest of the request.
      for (const field of ["run_at_ms", "delay"]) {
        if (args[field] !== undefined) throw new ArgumentError("queue_http sends as soon as possible; use schedule_http for a later time", field);
      }
      only(args, ["url", "method", "headers", "body", "callback_url", "capture", "idempotency_key", "max_attempts", "queue"]);
      const { queue, ...request } = args;
      if (queue === undefined) return { method: "POST", path: "/v1/tasks", scope: "schedule", body: request };
      if (typeof queue !== "string" || !new RegExp(LANE_PATTERN).test(queue)) throw new ArgumentError("invalid queue", "queue");
      return { method: "POST", path: "/v1/tasks", scope: "schedule", body: { ...request, lane: queue } };
    }
    case "get_task":
      return { method: "GET", path: `/v1/tasks/${taskId(args)}`, scope: "read" };
    case "list_tasks":
      return { method: "GET", path: `/v1/tasks${listQuery(args)}`, scope: "read" };
    case "cancel_task":
      return { method: "POST", path: `/v1/tasks/${taskId(args)}/cancel`, scope: "cancel" };
    case "redrive_task":
      return { method: "POST", path: `/v1/tasks/${taskId(args)}/redrive`, scope: "schedule" };
    case "create_cron":
      // The API validates the cron fully and names the failing field.
      return { method: "POST", path: "/v1/crons", scope: "schedule", body: args };
    case "get_cron":
      return { method: "GET", path: `/v1/crons/${cronName(args)}`, scope: "read" };
    case "list_crons":
      only(args, []);
      return { method: "GET", path: "/v1/crons", scope: "read" };
    case "delete_cron":
      return { method: "POST", path: `/v1/crons/${cronName(args)}/delete`, scope: "cancel" };
    case "watch_http":
      // The API validates the watch fully and names the failing field.
      return { method: "POST", path: "/v1/watches", scope: "schedule", body: args };
    case "read_inbox":
      // Acking is part of reading: it moves the project's one cursor. The API validates the rest.
      return { method: "POST", path: "/v1/inbox/read", scope: "read", body: args };
    case "ack_inbox":
      return { method: "POST", path: "/v1/inbox/ack", scope: "read", body: args };
    case "set_wake":
      // The API validates the wake fully and names the failing field.
      return { method: "POST", path: "/v1/wake", scope: "schedule", body: args };
    case "delete_wake":
      only(args, []);
      return { method: "POST", path: "/v1/wake/delete", scope: "cancel", body: {} };
    case "set_secret":
      // The API validates the secret fully and names the failing field; it never echoes the value.
      return { method: "POST", path: "/v1/secrets", scope: "secrets", body: args };
    case "list_secrets":
      only(args, []);
      return { method: "GET", path: "/v1/secrets", scope: "read" };
    case "delete_secret":
      return { method: "POST", path: `/v1/secrets/${secretName(args)}/delete`, scope: "secrets", body: {} };
    case "create_hook":
      // The API validates the hook fully and names the failing field; it never echoes the secret.
      return { method: "POST", path: "/v1/hooks", scope: "schedule", body: args };
    case "list_hooks":
      only(args, []);
      return { method: "GET", path: "/v1/hooks", scope: "read" };
    case "update_hook": {
      const { name, ...changes } = args;
      // The API validates the changes fully and names the failing field.
      return { method: "POST", path: `/v1/hooks/${hookName({ name })}`, scope: "schedule", body: changes };
    }
    case "delete_hook":
      return { method: "POST", path: `/v1/hooks/${hookName(args)}/delete`, scope: "cancel", body: {} };
    default:
      throw new ArgumentError(`unknown capability ${capability}`);
  }
}

export async function execute(client: ApiClient, capability: string, args: Args): Promise<unknown> {
  const r = route(capability, args);
  return client.call(r.method, r.path, r.scope, r.body);
}
