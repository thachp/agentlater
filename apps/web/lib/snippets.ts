// Code shown in the docs. test/snippets.test.ts executes the verification samples against
// the signature test vector in crates/egress, so they cannot silently go wrong.

import { GATEWAY_URL } from "./site";

export const mcpRemoteConfig = `{
  "mcpServers": {
    "runlater": { "url": "${GATEWAY_URL}/mcp" }
  }
}`;

export const mcpClaudeCode = `claude mcp add --transport http runlater ${GATEWAY_URL}/mcp`;

export const mcpConfig = `{
  "mcpServers": {
    "runlater": {
      "command": "npx",
      "args": ["@auth/agent-cli@0.5.1", "mcp", "--url", "${GATEWAY_URL}"],
      "env": { "AGENT_AUTH_ENCRYPTION_KEY": "<32 random bytes, hex>" }
    }
  }
}`;

export const sdkExample = `import { AgentAuthClient, MemoryStorage } from "@auth/agent";

const client = new AgentAuthClient({
  storage: new MemoryStorage(), // use persistent storage: losing the keys loses the project
  directoryUrl: null,
  allowDirectDiscovery: true,
});

// 1. Register an identity. No account, no API key.
const { agentId } = await client.connectAgent({
  provider: "${GATEWAY_URL}",
  mode: "autonomous",
  name: "my agent",
  capabilities: ["create_project", "get_project", "schedule_http", "get_task",
                 "list_tasks", "cancel_task", "redrive_task",
                 "create_cron", "get_cron", "list_crons", "delete_cron", "watch_http",
                 "read_inbox", "ack_inbox", "set_wake", "delete_wake",
                 "set_secret", "list_secrets", "delete_secret",
                 "create_hook", "list_hooks", "delete_hook"],
});
const run = (capability, args = {}) =>
  client.executeCapability({ agentId, capability, arguments: args }).then((r) => r.data);

// 2. Create the project once. Keep signing_secret for the destination.
const project = await run("create_project");

// 3. Schedule a request for tomorrow.
const task = await run("schedule_http", {
  url: "https://example.com/hooks/reminder",
  body: JSON.stringify({ remind: "invoice #42" }),
  headers: { "Content-Type": "application/json" },
  run_at_ms: Date.now() + 24 * 60 * 60 * 1000,
  idempotency_key: "reminder-invoice-42",
});

// 4. Later: did it happen?
const status = await run("get_task", { task_id: task.id });
console.log(status.status, status.attempts);`;

export const scheduleCall = `schedule_http({
  url: "https://example.com/hooks/reminder",
  body: "{\\"remind\\":\\"invoice #42\\"}",
  delay: "24h"
})`;

export const delayCall = `schedule_http({
  url: "https://example.com/follow-up",
  delay: "24h",
  idempotency_key: "follow-up-42"
})`;

export const laneCall = `schedule_http({
  url: "https://example.com/sync",
  lane: "user-42",
  callback_url: "https://example.com/done"
})`;

export const cronCall = `create_cron({
  name: "nightly-report",
  schedule: "0 6 * * *",
  url: "https://example.com/report",
  alert_url: "https://example.com/alert"
})`;

export const taskResult = `{
  "id": "task_3bc148a4ebabd050a7bf",
  "status": "succeeded",
  "attempt_count": 2,
  "attempts": [
    { "number": 1, "outcome": "http_503" },
    { "number": 2, "outcome": "http_200" }
  ]
}`;

export const deliveryHeaders = `POST /hooks/reminder HTTP/1.1
Host: example.com
Content-Type: application/json
User-Agent: RunLater/1 (+https://runlater.io)
RunLater-Task-Id: task_3bc148a4ebabd050a7bf
RunLater-Attempt: 1
RunLater-Timestamp: 1767312000
RunLater-Signature: v1=5d0e…c41a

{"remind":"invoice #42"}`;

export const verifyNode = `import { createHmac, timingSafeEqual } from "node:crypto";

// headers: lowercase names. rawBody: the body exactly as received (string or Buffer).
// Returns false for anything malformed; never throws on request input.
export function verifyRunLater(headers, rawBody, secret, toleranceSecs = 300) {
  const taskId = headers["runlater-task-id"];
  const attempt = headers["runlater-attempt"];
  const timestamp = headers["runlater-timestamp"];
  if (!taskId || !attempt || !/^[0-9]+$/.test(timestamp ?? "")) return false;
  if (Math.abs(Date.now() / 1000 - Number(timestamp)) > toleranceSecs) return false;
  const expected = createHmac("sha256", secret)
    .update(\`\${timestamp}.\${taskId}.\${attempt}.\`)
    .update(rawBody)
    .digest();
  // During secret rotation the header holds several comma-separated v1= entries.
  return String(headers["runlater-signature"] ?? "").split(",").some((entry) => {
    const [scheme, hex] = entry.trim().split("=");
    return scheme === "v1" && /^[0-9a-f]{64}$/.test(hex ?? "") &&
      timingSafeEqual(Buffer.from(hex, "hex"), expected);
  });
}`;

export const verifyPython = `import hashlib
import hmac
import re
import time

_HEX64 = re.compile(r"[0-9a-f]{64}")
_DIGITS = re.compile(r"[0-9]+")


def verify_runlater(headers, raw_body: bytes, secret: str, tolerance_secs: float = 300) -> bool:
    """headers: lowercase names. raw_body: the body exactly as received.
    Returns False for anything malformed; never raises on request input."""
    task_id = headers.get("runlater-task-id")
    attempt = headers.get("runlater-attempt")
    timestamp = headers.get("runlater-timestamp") or ""
    if not task_id or not attempt or not _DIGITS.fullmatch(timestamp):
        return False
    if abs(time.time() - int(timestamp)) > tolerance_secs:
        return False
    prefix = f"{timestamp}.{task_id}.{attempt}.".encode()
    expected = hmac.new(secret.encode(), prefix + raw_body, hashlib.sha256).hexdigest()
    # During secret rotation the header holds several comma-separated v1= entries.
    for entry in (headers.get("runlater-signature") or "").split(","):
        scheme, _, sig = entry.strip().partition("=")
        if scheme == "v1" and _HEX64.fullmatch(sig) and hmac.compare_digest(sig, expected):
            return True
    return False`;
