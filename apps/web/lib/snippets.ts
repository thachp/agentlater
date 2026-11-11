// Code shown in the docs. test/snippets.test.ts executes the verification samples against
// the signature test vector in crates/egress, so they cannot silently go wrong.

import { GATEWAY_URL } from "./site";

export const mcpRemoteConfig = `{
  "mcpServers": {
    "runlater": { "url": "${GATEWAY_URL}/mcp" }
  }
}`;

export const mcpClaudeCode = `claude mcp add --transport http runlater ${GATEWAY_URL}/mcp`;

/** The landing page's Claude row: Claude Code, then Claude Desktop and claude.ai. */
export const mcpClaude = `# Claude Code
claude mcp add --transport http runlater ${GATEWAY_URL}/mcp

# Claude Desktop or claude.ai
Settings → Connectors → Add custom connector
  URL: ${GATEWAY_URL}/mcp`;

export const mcpChatGpt = `chatgpt.com/plugins → + → Add custom MCP server
  URL:            ${GATEWAY_URL}/mcp
  Authentication: OAuth`;

export const mcpCodex = `# ~/.codex/config.toml
[mcp_servers.runlater]
url = "${GATEWAY_URL}/mcp"

# then sign in once
codex mcp login runlater`;

export const mcpGrok = `grok.com → Connectors → Custom
  Server URL: ${GATEWAY_URL}/mcp`;

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
  capabilities: ["create_project", "get_project", "rotate_signing_secret", "get_payment_link",
                 "schedule_http", "queue_http",
                 "get_task", "list_tasks", "cancel_task", "redrive_task",
                 "create_cron", "get_cron", "list_crons", "delete_cron", "watch_http",
                 "read_inbox", "ack_inbox", "set_wake", "delete_wake",
                 "set_secret", "list_secrets", "delete_secret",
                 "create_hook", "list_hooks", "update_hook", "delete_hook"],
});
const run = (capability, args = {}) =>
  client.executeCapability({ agentId, capability, arguments: args }).then((r) => r.data);

// 2. Create the project once. Keep signing_secret for the destination:
//    create_project returns it (calling again returns it again).
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

/** The landing page's short version of sdkExample. */
export const sdkShort = `import { AgentAuthClient } from "@auth/agent";

const client = new AgentAuthClient({
  storage, // persistent: losing the keys loses the project
  directoryUrl: null,
  allowDirectDiscovery: true,
});
const { agentId } = await client.connectAgent({
  provider: "${GATEWAY_URL}",
  mode: "autonomous",
  name: "my agent",
  // the full list is in the quickstart
  capabilities: ["create_project", "schedule_http", "watch_http", "read_inbox"],
});
const run = (capability, args = {}) =>
  client.executeCapability({ agentId, capability, arguments: args });
await run("create_project");`;

export const agentPrompt = `Use AgentLater to watch https://deploy.example.com/deployments/123
every 5 minutes. When status is SUCCEEDED, start the smoke tests at
https://ci.example.com/smoke and put the result in the inbox.
Next session, read the inbox first.`;

export const watchActCall = `watch_http({
  url: "https://deploy.example.com/deployments/123",
  every: "5m",
  deadline: "6h",
  until: { path: "/status", in: ["SUCCEEDED"] },
  on_match: {
    url: "https://ci.example.com/smoke",
    headers: { "Authorization": "Bearer {{secret:CI_TOKEN}}" },
    callback_url: "inbox"
  }
})`;

export const hookCall = `create_hook({
  name: "ci-failures",
  verify: { scheme: "github", secret: "…" },
  accept: [{
    headers: { "X-GitHub-Event": ["workflow_run"] },
    path: "/workflow_run/conclusion",
    in: ["failure"]
  }]
})
// → { url: "https://hooks.agentlater.io/h/prj_…/hk_…" }`;

export const inboxCall = `// next session
get_project()  // → { inbox_unread: 2, … }
read_inbox({ ack: true })
// → [{ event: "task.finished", task: { status: "succeeded",
//      response: { status: 200, body: "{\\"passed\\":42}" } } },
//    { event: "hook.received", hook: "ci-failures", … }]`;

export const scheduleCall = `schedule_http({
  url: "https://example.com/remind",
  delay: "24h",
  callback_url: "inbox"
})`;

export const readInboxCall = `read_inbox({ ack: true })
// → task.finished
//   status: "succeeded"`;

export const deliveryHeaders = `POST /hooks/reminder HTTP/1.1
Host: example.com
Content-Type: application/json
User-Agent: AgentLater/1 (+https://agentlater.io)
RunLater-Task-Id: task_3bc148a4ebabd050a7bf
RunLater-Attempt: 1
RunLater-Timestamp: 1767312000
RunLater-Signature: v1=5d0e…c41a

{"remind":"invoice #42"}`;

export const rotateSigningSecret = `// The signing secret leaked: the old one stops signing from the next attempt
// (one already in flight may still carry it for a few seconds).
rotate_signing_secret({ overlap: "0s" })
// { signing_secret: "rls_9f2c…", previous_valid_until_ms: null }

// Routine: both signing secrets sign for the overlap (default "24h", at most "7d").
rotate_signing_secret({ overlap: "2h" })
// { signing_secret: "rls_41d7…", previous_valid_until_ms: 1767319200000 }

// During the overlap a delivery carries one entry per secret:
// RunLater-Signature: v1=8a13…07be,v1=5d0e…c41a`;

export const verifyNode = `import { createHmac, timingSafeEqual } from "node:crypto";

// headers: lowercase names. rawBody: the body exactly as received (string or Buffer).
// Returns false for anything malformed; never throws on request input.
export function verifyAgentLater(headers, rawBody, secret, toleranceSecs = 300) {
  const taskId = headers["runlater-task-id"];
  const attempt = headers["runlater-attempt"];
  const timestamp = headers["runlater-timestamp"];
  if (!taskId || !attempt || !/^[0-9]+$/.test(timestamp ?? "")) return false;
  if (Math.abs(Date.now() / 1000 - Number(timestamp)) > toleranceSecs) return false;
  const expected = createHmac("sha256", secret)
    .update(\`\${timestamp}.\${taskId}.\${attempt}.\`)
    .update(rawBody)
    .digest();
  // After rotate_signing_secret the header holds one v1= entry per signing secret until the
  // overlap ends: accept a match on any.
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
    # After rotate_signing_secret the header holds one v1= entry per signing secret until the
    # overlap ends: accept a match on any.
    for entry in (headers.get("runlater-signature") or "").split(","):
        scheme, _, sig = entry.strip().partition("=")
        if scheme == "v1" and _HEX64.fullmatch(sig) and hmac.compare_digest(sig, expected):
            return True
    return False`;
