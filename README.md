# AgentLater

Let an AI agent make an HTTP request later, and check whether it happened.

AgentLater is a hosted [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) server for
agents that need to schedule HTTP work, wait for a job, or retrieve results in a later session.
It sends requests and records delivery outcomes; it does not run the agent itself.

## What agents can do

| Need | Tools |
| --- | --- |
| Send a request later or in the background | `schedule_http`, `queue_http` (optionally serialize related requests in a named queue) |
| Repeat a request or poll until a job finishes | `create_cron`, `watch_http` |
| Receive webhooks and read results in a later session | `create_hook`, `read_inbox`, `ack_inbox` |
| Inspect, stop or retry work | `get_task`, `list_tasks`, `cancel_task`, `redrive_task` |
| Use credentials or trigger an agent workflow when results arrive | `set_secret`, `set_wake` |

See the [full tool reference](docs/spec/PRODUCT.md) for arguments, limits and delivery semantics.
A wake URL must belong to something that can start an agent run; inbox events alone do not resume a chat.

## Connect an MCP client

Use a client that supports **remote Streamable HTTP and MCP OAuth**. Add a remote server or custom
connector named `agentlater` with this URL:

```text
https://gateway.agentlater.io/mcp
```

For clients using an `mcpServers` JSON configuration with HTTP transport, the configuration is:

```json
{
  "mcpServers": {
    "agentlater": {
      "type": "http",
      "url": "https://gateway.agentlater.io/mcp"
    }
  }
}
```

Configuration formats vary by client; use its remote-server settings if it does not accept this shape.
The endpoint and transport are also declared in [server.json](server.json).

**Authentication:** complete the OAuth flow opened by your client and click **Connect** on the
AgentLater consent page. There is no account to create or API key to paste into the configuration;
the client manages OAuth tokens. Each approved connection owns a separate project and trial.
Call `create_project` once after connecting (calling it again returns the same project): the trial
includes 1,000 delivery attempts for 7 days. Use `get_project` to inspect the balance and `get_payment_link`
when more attempts are needed.

For **Claude Code**, you can instead install the plugin, which includes the hosted server configuration
and a skill for using the tools:

```text
/plugin marketplace add thachp/agentlater
/plugin install agentlater@agentlater
```

See the [plugin README](plugins/agentlater/README.md) for installation and migration details.
Autonomous integrations can also use [Agent Auth](docs/spec/PRODUCT.md#capabilities-the-api-surface) at the gateway;
that identity and its project are separate from a hosted MCP connection.

## Example: trigger a background job through a webhook

A common use is calling your application's job endpoint after a delay—for example, generating
an export in 15 minutes without keeping the agent session open.

After connecting, ask your agent:

> Create my AgentLater project, then POST to my export webhook in 15 minutes with
> `{"report": "daily-sales", "format": "csv"}`. Use my stored webhook token, capture the response,
> and put the delivery result in my inbox.

The agent calls `create_project` with `{}`. If the endpoint requires a bearer token, first call
`set_secret` with these arguments, replacing the placeholder with your endpoint's actual token
and the host with your own:

```json
{
  "name": "WEBHOOK_TOKEN",
  "value": "<your-webhook-token>",
  "allowed_hosts": ["api.example.com"]
}
```

Then call `schedule_http` with the following arguments. Replace the example URL with your own
public HTTPS job endpoint. `body` is a JSON-encoded **string**, not an object.

```json
{
  "url": "https://api.example.com/jobs/export",
  "method": "POST",
  "headers": {
    "Content-Type": "application/json",
    "Authorization": "Bearer {{secret:WEBHOOK_TOKEN}}"
  },
  "body": "{\"report\":\"daily-sales\",\"format\":\"csv\"}",
  "delay": "15m",
  "idempotency_key": "daily-sales-export-001",
  "capture": true,
  "callback_url": "inbox"
}
```

Use a new `idempotency_key` for each intended job; reusing the same key and request returns the
original task. Your endpoint should also deduplicate deliveries using `RunLater-Task-Id` because
delivery retries can reach it more than once. For an endpoint without authentication, omit the
secret setup and the `Authorization` header.

Save the returned `task_id` and inspect it with `get_task({"task_id": "<returned task_id>"})`.
In a later session, call `get_project` with `{}`, then `read_inbox` with `{}` if `inbox_unread`
is above zero. After handling the events, call `ack_inbox({"up_to": 1})`, replacing `1` with the
last `seq` you handled. These are MCP tool names and argument objects, not shell commands.
No public callback URL is needed to receive the delivery report in the inbox. If the endpoint
returns `202 Accepted`, that confirms it accepted the job; use `watch_http` on its status URL
when you also need to wait for the export to finish.

Delivery is **at least once**; `succeeded` means the destination returned HTTP 2xx. Destinations must
be public HTTPS URLs, not localhost or private IP addresses. For authenticated requests, store the
credential with `set_secret` and use `{{secret:NAME}}` in a header instead of pasting it into a request.

## Limits and troubleshooting

- Schedule at most 7 days ahead. Cron expressions use UTC and run at most hourly; watches poll
  every 5 minutes to 24 hours. Captured responses keep the first 16 KiB of the body for 7 days
  after the task finishes.
- `project_not_found`: call `create_project` on the current connection before scheduling.
- `budget_exhausted` or `trial_expired`: inspect `get_project` and use `get_payment_link` for more
  attempts. Each delivery attempt, poll and URL callback attempt consumes budget; inbox delivery is free.
- Connection or sign-in fails: confirm the URL ends in `/mcp` and the client supports both
  Streamable HTTP and OAuth. A client limited to local stdio servers needs a supported bridge;
  see the [Agent Auth bridge configuration](docs/spec/PRODUCT.md#capabilities-the-api-surface).
- No result yet: inspect `get_task` with the returned `task_id` for its status and attempt history.
  Read the inbox using the same connection that scheduled the task; a new connection has a separate project.

## Documentation and compatibility

AgentLater was formerly RunLater. Existing `runlater` plugin connections retain their own projects;
installing the `agentlater` plugin creates a separate connection rather than moving existing work.
Some internal package names and delivery headers (such as `RunLater-Task-Id`) retain the old name
for compatibility.

The specs are [PRODUCT.md](docs/spec/PRODUCT.md) (what it does), [ARCHITECTURE.md](docs/spec/ARCHITECTURE.md)
(how it's built) and [DESIGN.md](docs/spec/DESIGN.md) (how it should feel, to agents and to people). The pre-rebuild implementation was removed at
cutover; it is in git history before this change.

## Layout

| Path | What | Stack |
| --- | --- | --- |
| `apps/api` | Private task API: ledger, scheduling, delivery | Rust · workers-rs |
| `apps/gateway` | Public entry for agents: Agent Auth, capabilities, MCP | TypeScript Worker |
| `apps/web` | agentlater.io landing, docs and payment page | Next.js · OpenNext |
| `apps/receiver` | Staging-only fixture the journey delivers to | TypeScript Worker |
| `crates/core` | Task domain, no Cloudflare deps | Rust |
| `crates/egress` | Destination policy and request signing | Rust |
| `packages/contracts` | Capability definitions, and the OpenAPI contract between gateway and API | TypeScript · OpenAPI 3.1 |
| `packages/tsconfig` | Shared TypeScript config | |
| `plugins/agentlater` | Claude Code plugin: the hosted MCP server and an `agentlater` skill | Markdown · JSON |
| `plugins/runlater` | The old plugin, deprecated and frozen so existing installs keep working | Markdown · JSON |

Everything deploys to Cloudflare Workers, with `staging` and `production` environments in each app's `wrangler.jsonc` (`apps/receiver` has staging only); see [docs/DEPLOY.md](docs/DEPLOY.md).

## Develop

Requires Rust (pinned in `rust-toolchain.toml`), Node 22+, and `cargo install worker-build --version 0.8.7 --locked`.

```bash
npm install
```

```bash
cargo test && npm run typecheck && npm test
```

Every check CI runs, in one command (`-- --fast` skips the e2e suites):

```bash
npm run verify
```

The rules the code keeps, and the lints that enforce them, are in [CODING_STANDARDS.md](CODING_STANDARDS.md).

```bash
npm run dev -w @runlater/gateway
```

```bash
npm run dev -w @runlater/web
```

## License

The code is [MIT](LICENSE). Using the hosted service at agentlater.io is covered by its [Terms of Service](https://agentlater.io/terms) and [Privacy Policy](https://agentlater.io/privacy).
