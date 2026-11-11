import { describe, expect, it } from "vitest";
import { ArgumentError, capabilities, capabilityNames, route } from "../src/capabilities";

const ID = "task_0123456789abcdef0123";

describe("capabilities", () => {
  it("match the product spec", () => {
    expect(capabilityNames).toEqual([
      "create_project",
      "get_project",
      "rotate_signing_secret",
      "get_payment_link",
      "schedule_http",
      "queue_http",
      "get_task",
      "list_tasks",
      "cancel_task",
      "redrive_task",
      "create_cron",
      "get_cron",
      "list_crons",
      "delete_cron",
      "watch_http",
      "read_inbox",
      "ack_inbox",
      "set_wake",
      "delete_wake",
      "set_secret",
      "list_secrets",
      "delete_secret",
      "create_hook",
      "list_hooks",
      "update_hook",
      "delete_hook",
    ]);
    for (const c of capabilities) {
      expect(c.description.length).toBeGreaterThan(20);
      expect(c.input).toMatchObject({ type: "object", additionalProperties: false });
    }
  });

  it("map to API routes with the narrowest scope", () => {
    expect(route("create_project", {})).toEqual({ method: "POST", path: "/v1/projects", scope: "project" });
    expect(route("get_project", {})).toMatchObject({ path: "/v1/projects/self", scope: "read" });
    expect(route("schedule_http", { url: "https://a.example.com/" })).toEqual({
      method: "POST",
      path: "/v1/tasks",
      scope: "schedule",
      body: { url: "https://a.example.com/" },
    });
    expect(route("queue_http", { url: "https://a.example.com/", queue: "payments" })).toEqual({
      method: "POST",
      path: "/v1/tasks",
      scope: "schedule",
      body: { url: "https://a.example.com/", lane: "payments" },
    });
    expect(route("queue_http", { url: "https://a.example.com/" }).body).toEqual({ url: "https://a.example.com/" });
    expect(route("get_task", { task_id: ID })).toMatchObject({ method: "GET", path: `/v1/tasks/${ID}`, scope: "read" });
    expect(route("cancel_task", { task_id: ID })).toMatchObject({ path: `/v1/tasks/${ID}/cancel`, scope: "cancel" });
    expect(route("redrive_task", { task_id: ID })).toMatchObject({ path: `/v1/tasks/${ID}/redrive`, scope: "schedule" });
    expect(route("create_cron", { name: "b" })).toMatchObject({ method: "POST", path: "/v1/crons", scope: "schedule" });
    expect(route("get_cron", { name: "daily-backup" })).toMatchObject({ method: "GET", path: "/v1/crons/daily-backup", scope: "read" });
    expect(route("list_crons", {})).toMatchObject({ method: "GET", path: "/v1/crons", scope: "read" });
    expect(route("delete_cron", { name: "a:b.c" })).toMatchObject({ path: "/v1/crons/a:b.c/delete", scope: "cancel" });
    expect(route("watch_http", { url: "https://a.example.com/" })).toMatchObject({ method: "POST", path: "/v1/watches", scope: "schedule" });
    const onMatch = { url: "https://ci.example.com/smoke", body: "{}" };
    expect(route("watch_http", { url: "https://a.example.com/", on_match: onMatch }).body).toMatchObject({ on_match: onMatch });
    expect(route("read_inbox", { ack: true })).toEqual({ method: "POST", path: "/v1/inbox/read", scope: "read", body: { ack: true } });
    expect(route("ack_inbox", { up_to: 3 })).toEqual({ method: "POST", path: "/v1/inbox/ack", scope: "read", body: { up_to: 3 } });
    expect(route("set_wake", { url: "https://a.example.com/" })).toMatchObject({ method: "POST", path: "/v1/wake", scope: "schedule" });
    expect(route("delete_wake", {})).toEqual({ method: "POST", path: "/v1/wake/delete", scope: "cancel", body: {} });
    expect(() => route("delete_wake", { url: "x" })).toThrow(ArgumentError);
    expect(route("set_secret", { name: "GH" })).toMatchObject({ method: "POST", path: "/v1/secrets", scope: "secrets" });
    expect(route("list_secrets", {})).toEqual({ method: "GET", path: "/v1/secrets", scope: "read" });
    expect(route("delete_secret", { name: "GH_TOKEN" })).toEqual({ method: "POST", path: "/v1/secrets/GH_TOKEN/delete", scope: "secrets", body: {} });
    for (const name of ["gh", "../x", "GH/delete", 7]) expect(() => route("delete_secret", { name })).toThrow(ArgumentError);
    expect(route("create_hook", { name: "gh-ci" })).toMatchObject({ method: "POST", path: "/v1/hooks", scope: "schedule", body: { name: "gh-ci" } });
    expect(route("list_hooks", {})).toEqual({ method: "GET", path: "/v1/hooks", scope: "read" });
    expect(route("update_hook", { name: "gh-ci", accept: null })).toEqual({ method: "POST", path: "/v1/hooks/gh-ci", scope: "schedule", body: { accept: null } });
    for (const name of ["_receive", "a/b", undefined]) expect(() => route("update_hook", { name, accept: null })).toThrow(ArgumentError);
    expect(route("delete_hook", { name: "gh-ci" })).toEqual({ method: "POST", path: "/v1/hooks/gh-ci/delete", scope: "cancel", body: {} });
    for (const name of ["_receive", "../x", "a/b", 7]) expect(() => route("delete_hook", { name })).toThrow(ArgumentError);
  });

  it("build list queries", () => {
    expect(route("list_tasks", {}).path).toBe("/v1/tasks");
    expect(route("list_tasks", { status: "failed", cursor: "42", limit: 5 }).path).toBe(
      "/v1/tasks?status=failed&cursor=42&limit=5",
    );
  });

  it("refuse task ids that could escape the path", () => {
    for (const task_id of ["../projects/self", "task_x", ID + "/cancel", 7]) {
      expect(() => route("get_task", { task_id })).toThrow(ArgumentError);
    }
    for (const name of ["..", ".", "a/b", "a?x", "", "x".repeat(65), 7]) {
      expect(() => route("get_cron", { name })).toThrow(ArgumentError);
      expect(() => route("delete_cron", { name })).toThrow(ArgumentError);
    }
  });

  it("accept every argument queue_http advertises", () => {
    const { properties } = capabilities.find((c) => c.name === "queue_http")!.input;
    for (const field of Object.keys(properties)) {
      expect(() => route("queue_http", { url: "https://a.example.com/", [field]: field === "url" ? "https://a.example.com/" : "x" })).not.toThrow(/unknown argument/);
    }
    // Everything but the queue goes to the API as given, and schedule_http takes the same fields.
    const schedule = capabilities.find((c) => c.name === "schedule_http")!.input.properties;
    for (const field of Object.keys(properties)) if (field !== "queue") expect(schedule[field]).toEqual(properties[field]);
  });

  it("refuse unknown or invalid arguments", () => {
    expect(() => route("get_project", { project_id: "other" })).toThrow(/unknown argument/);
    expect(() => route("get_task", { task_id: ID, extra: 1 })).toThrow(ArgumentError);
    expect(() => route("list_tasks", { status: "done" })).toThrow(ArgumentError);
    expect(() => route("list_tasks", { cursor: "1&status=x" })).toThrow(ArgumentError);
    expect(() => route("list_tasks", { limit: 500 })).toThrow(ArgumentError);
    expect(() => route("list_tasks", { lane: "a&status=x" })).toThrow(ArgumentError);
    expect(route("list_tasks", { lane: "payments:eu" }).path).toBe("/v1/tasks?lane=payments%3Aeu");
    expect(route("list_tasks", { queue: "payments:eu" }).path).toBe("/v1/tasks?lane=payments%3Aeu");
    expect(() => route("list_tasks", { queue: "a&status=x" })).toThrow(ArgumentError);
    expect(() => route("list_tasks", { queue: "a", lane: "b" })).toThrow(ArgumentError);
    expect(() => route("queue_http", { url: "https://a.example.com/", delay: "1h" })).toThrow(/schedule_http/);
    expect(() => route("queue_http", { url: "https://a.example.com/", run_at_ms: 1 })).toThrow(/schedule_http/);
    expect(() => route("queue_http", { url: "https://a.example.com/", lane: "a" })).toThrow(/takes queue/);
    expect(() => route("queue_http", { url: "https://a.example.com/", extra: 1 })).toThrow(/unknown argument/);
    expect(route("queue_http", { url: "https://a.example.com/", queue: null, delay: null, run_at_ms: null }).body).toEqual({
      url: "https://a.example.com/",
    });
    expect(route("list_tasks", { queue: null, lane: "a" }).path).toBe("/v1/tasks?lane=a");
    expect(route("list_tasks", { queue: "a", lane: null }).path).toBe("/v1/tasks?lane=a");
    expect(() => route("queue_http", { url: "https://a.example.com/", queue: "a b" })).toThrow(ArgumentError);
    expect(() => route("nope", {})).toThrow(ArgumentError);
  });
});
