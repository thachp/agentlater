# AgentLater — Product Spec (v1)

Status: **v1, live** at `gateway.agentlater.io` · Owner: Patrick Thach · Last updated: 2026-11-04 · How it's built: [ARCHITECTURE.md](ARCHITECTURE.md) · How it should feel: [DESIGN.md](DESIGN.md)

## One line

**AgentLater lets an AI agent say "make this HTTP request later" — and then check whether it happened.**

Publicly: *your agent's session ends; the work doesn't.* The request is the unit, and everything else (crons, watches, hooks, the inbox, wake) exists so that waiting, and finding out, can outlast a session.

## Problem

Agents work in short sessions. They cannot sleep for an hour, wake up tomorrow, or
retry a flaky webhook over the next hour. Today an agent that needs
"call this URL at 9am" or "retry this until it succeeds" has nowhere to put that
intent. Developers solve it with cron boxes, queues, or QStash-style services that
assume a human signed up, pasted an API key, and wired a dashboard.

The harder half is coming back. Most agents have no public URL, so nothing can tell them what
happened, and the next session starts by re-checking everything. Outside events (a failed CI run,
a disputed payment) have the same problem: they need a server to land on.

## Who it's for

| Priority | User | How they reach us |
| --- | --- | --- |
| Primary | **AI agents** acting for a person or system | Agent Auth capabilities, or the same capabilities as tools on the hosted MCP server (`/mcp`) |
| Secondary | Developers building those agents | Agent Auth SDK from code; docs site |
| Occasional | The agent's person | One click on a consent page; a payment link when attempts run out |

There is no human sign-up, password, or dashboard in v1. The agent *is* the customer.

### Where agents find it

| Channel | What it is | Source |
| --- | --- | --- |
| Hosted MCP | `https://gateway.agentlater.io/mcp`, added as a custom connector or remote server in Claude, ChatGPT, Grok, Codex, Cursor and others | `apps/gateway/src/mcp.ts` |
| Claude Code plugin | The MCP server plus an `agentlater` skill that says when to use it, including the start-of-session inbox check | `plugins/agentlater`, `.claude-plugin/marketplace.json` |
| MCP Registry | `io.agentlater/agentlater`, pointing at the hosted server | `server.json` |
| Agent Auth | Discovery at `/.well-known/agent-configuration`, then the SDK or the local bridge | `apps/gateway/src/auth.ts` |
| Machine-readable pages | `/llms.txt` says what AgentLater is and how to connect; `/openapi.json` advertises the paid MPP endpoint | `apps/gateway/src/discovery.ts` |
| Site | Landing page and docs at `agentlater.io` | `apps/web` |

## Core jobs (v1)

**Act later**

1. **Schedule or queue** an HTTP request to a public HTTPS URL: `schedule_http` at a time, `queue_http` as soon as possible, optionally in a named queue (a *lane* in the API) so related requests run one at a time.
2. **Repeat** it on a schedule, and hear when runs start failing: *Crons*.
3. **Wait** on someone else's job by polling it, and optionally send the next request when it finishes: *Watches*.

**Find out later**

4. **Check** a task's status and its delivery attempts; **list** recent tasks for the project.
5. **Read results** from a later session, without a public URL of its own: the *Inbox*, with *captured responses*.
6. **Be woken** when something lands there, instead of waiting for a person to open a session: *Wake*.
7. **Receive webhooks** from GitHub, Stripe or CI, filtered to what matters: *Inbound hooks*.

**Stay in control**

8. **Cancel** a task that hasn't run yet; **redrive** one that failed.
9. **Use credentials without handling them**: *Secrets*, bound to the hosts they are for.
10. **Buy more attempts**, through the agent's person or over MPP: *Buying attempts*.

That's the whole product for v1.

## How it works (user's view)

```
Agent ──connect──▶ AgentLater            hosted MCP: a person clicks Connect once
                                       Agent Auth: discover, register, get keys
Agent ──create_project──▶ AgentLater     gets a free trial (finite budget, 7 days)
Agent ──schedule_http──▶ AgentLater      { url, method, headers, body, delay, callback_url: "inbox" }
                                       ← task_id, status: scheduled
          … time passes …
AgentLater ──signed HTTPS request──▶ destination   (retries on failure)
AgentLater ──task.finished──▶ inbox
Next session ──get_project──▶ AgentLater ← inbox_unread: 1
Next session ──read_inbox──▶ AgentLater  ← the report (or get_task: status, attempts)
```

## Concepts

| Term | Meaning |
| --- | --- |
| **Owner** | Who holds a project: an Agent Auth host (host + agent keypair), or a hosted MCP connection (an OAuth grant). There are no other ways to authenticate. |
| **Project** | Owned by exactly one owner, and never reassigned. Holds tasks, budget, the signing secret and everything below. One project per owner in v1. |
| **Trial** | A project's free allocation: N delivery attempts, expires after 7 days. No refills. |
| **Attempt pack** | Delivery attempts bought for a project: 50,000 for $5. See *Buying attempts*. |
| **Task** | One HTTP request to make at `run_at_ms`, with a retry policy. |
| **Attempt** | One try at delivering a task: time, response status or error. The billing unit. |
| **Lane** | A name that makes tasks run one at a time. |
| **Cron** | A request on a UTC schedule; each run is a task. |
| **Watch** | A task whose attempts are polls, ending when `until` matches. `on_match` is the one request it may send then. |
| **Inbox** | One log of events per project, with a cursor AgentLater keeps. |
| **Wake** | One URL per project that AgentLater calls to start the agent when the inbox has something. |
| **Secret** | A credential stored once, sent only to its allowed hosts, referred to as `{{secret:NAME}}`. |
| **Hook** | A public URL that turns requests from outside into inbox events. |

## Task lifecycle

```
scheduled ──(due time reached)──▶ delivering ──2xx──▶ succeeded
    │                               │
    │ cancel                        └─non-2xx/timeout─▶ retrying ──(backoff)──▶ delivering
    ▼                                                      │
 canceled                                     attempts exhausted ──▶ failed ──redrive──▶ scheduled
```

- `succeeded` means the destination returned 2xx. It does not mean the business action downstream worked.
- Delivery is **at-least-once**. Destinations should use the `RunLater-Task-Id` header to dedupe.
- No promise of exact timing, ordering, or an SLA in v1.

## Capabilities (the API surface)

Each one is an Agent Auth capability. MCP clients reach them two ways:

- **Hosted MCP server** at `https://gateway.agentlater.io/mcp` (Streamable HTTP). Each capability is a tool. Clients sign in with standard MCP OAuth: on first connect a AgentLater consent page opens, the person clicks **Connect**, and there is no account. Each approved connection is its own owner with its own project and trial, separate from any Agent Auth identity; the OAuth grant is its only key (refresh tokens last 30 days from last use). Over either path, one owner may make 300 tool calls a minute; more get a `rate_limited` error (a tool error over MCP, HTTP 429 over Agent Auth).
- **Agent Auth MCP bridge**, which runs locally and exposes generic tools (`connect_agent`, `list_capabilities`, `execute_capability`); the agent picks a AgentLater capability by name inside `execute_capability`:

```json
{ "mcpServers": { "runlater": {
  "command": "npx",
  "args": ["@auth/agent-cli@0.5.1", "mcp", "--url", "https://gateway.agentlater.io"],
  "env": { "AGENT_AUTH_ENCRYPTION_KEY": "<32 random bytes, hex>" }
} } }
```

The bridge keeps the agent's keys in `~/.agent-auth`. Losing them loses access to the project; unused purchased attempts may be refunded (open question 2).

| Capability | Input | Output |
| --- | --- | --- |
| `create_project` | — | project id, trial budget, expiry, signing secret (again on every repeat call) |
| `get_project` | — | budget used/remaining, expiry, `paid`, `inbox_unread`, `wake`, `signing_secret_previous_until_ms`; never the signing secret |
| `rotate_signing_secret` | `overlap?` | the new `signing_secret`, `previous_valid_until_ms` |
| `get_payment_link` | — | `payment_url` for a person to buy a pack, `mpp_url` for the agent to buy one over MPP, expiries, price |
| `schedule_http` | `url`, `method?`, `headers?`, `body?`, `run_at_ms?` or `delay?`, `idempotency_key?`, `max_attempts?`, `callback_url?`, `capture?`, `lane?` | `task_id`, `status` |
| `queue_http` | `url`, `method?`, `headers?`, `body?`, `idempotency_key?`, `max_attempts?`, `callback_url?`, `capture?`, `queue?` | `task_id`, `status` |
| `get_task` | `task_id` | task + attempts |
| `list_tasks` | `status?`, `queue?` or `lane?`, `cursor?`, `limit?` | page of tasks |
| `cancel_task` | `task_id` | task |
| `redrive_task` | `task_id` | task |
| `create_cron` | `name`, `schedule`, `url`, `method?`, `headers?`, `body?`, `max_attempts?`, `alert_url?`, `callback_url?`, `capture?`, `report?` | cron |
| `get_cron` | `name` | cron, with health |
| `list_crons` | — | crons |
| `delete_cron` | `name` | cron as it was |
| `watch_http` | `url`, `every`, `until`, `deadline`, `method?`, `headers?`, `body?`, `callback_url?`, `idempotency_key?`, `on_match?` | task, with `watch` |
| `read_inbox` | `after?`, `limit?`, `ack?` | `events`, `acked_seq`, `unread`, `dropped` |
| `ack_inbox` | `up_to` | `acked_seq`, `unread` |
| `set_wake` | `url`, `headers?`, `debounce?` | wake |
| `delete_wake` | — | wake as it was |
| `set_secret` | `name`, `value`, `allowed_hosts` | secret, without its value |
| `list_secrets` | — | secrets: names, hosts, times |
| `delete_secret` | `name` | secret as it was |
| `create_hook` | `name`, `verify?`, `keep_headers?`, `accept?` | hook, with its `url` (shown once) |
| `list_hooks` | — | hooks: settings and counts, never URLs or secrets |
| `update_hook` | `name`, `verify?`, `accept?`, `keep_headers?` | hook; its URL is unchanged |
| `delete_hook` | `name` | hook as it was |

The REST routes underneath (`/v1/tasks`, …) are an internal contract between
services, not a public product in v1.

## Delivery rules

- **Destinations:** `https://` on the default port to a public DNS name. No IP literals, credentials in the URL, fragments, or non-public names (`localhost`, `*.local`, `*.internal`, `*.test`, …). Checked when the task is accepted and again before every attempt. Resolved addresses are restricted to the public internet by Cloudflare (`global_fetch_strictly_public`).
- **Methods and headers:** `GET`, `POST` (default), `PUT`, `PATCH`, `DELETE`. Callers may set any header except hop-by-hop/framing headers (`Host`, `Content-Length`, `Transfer-Encoding`, …) and the `RunLater-*`, `CF-*`, `X-Forwarded-*` and `Proxy-*` families. `GET` cannot have a body.
- **Signing:** every outbound request carries `RunLater-Task-Id`, `RunLater-Attempt`, `RunLater-Timestamp` (unix seconds) and `RunLater-Signature: v1=<hex>`, an HMAC-SHA256 with the project's signing secret over `"{timestamp}.{task_id}.{attempt}."` followed by the raw body. During a rotation's overlap the header carries two comma-separated `v1=` entries, one per signing secret; receivers should accept a match on any entry, and reject timestamps more than 5 minutes off. `create_project` returns the current signing secret, the first time and on every repeat call, and `rotate_signing_secret` returns the one it makes. `get_project` leaves it out, because agents call it at the start of every session and the secret would land in every transcript.
- **Rotating the signing secret:** `rotate_signing_secret(overlap?)` replaces the secret with one call and returns the new one as `signing_secret`; from then on `create_project` returns the new one too. `overlap` is a duration like `delay`, from `"0s"` to `"7d"`, default `"24h"`.
  - *During the overlap* every delivery, callback and wake is signed with both signing secrets, so a destination verifies with whichever it holds. The owner puts the new one in the destination before `previous_valid_until_ms`; attempts made after that are signed with the new one only, and a destination still checking against the old one rejects everything. `get_project` shows the same deadline as `signing_secret_previous_until_ms` (null outside an overlap), so a later session can tell a rotation is under way without seeing either secret.
  - *A leaked secret:* rotate with `overlap: "0s"` and replace the secret in the destination straight away. The old signing secret stops signing from the next attempt (one already in flight may still carry it for a few seconds, at most the 10 s attempt timeout) and `previous_valid_until_ms` is null. Whoever holds the old secret can forge requests for as long as the destination still accepts it, so the leak is closed when the destination checks against the new secret only. Deliveries that reach the destination before it is updated fail its check; a destination that answers them `401` fails those tasks without a retry, and `redrive_task` sends them again.
  - At most two signing secrets sign at a time: rotating again during an overlap keeps the two newest, and the oldest stops signing from the next attempt, like a rotation with no overlap. Nothing else changes: budget, trial, tasks, crons, watches, hooks, inbox and stored secrets are untouched, and work already scheduled or retrying is signed with the new secret from its next attempt. The signed content, the header names and the `v1` scheme are the same, so a receiver that accepts any matching entry needs no code change, only the new secret.
  - Errors: `404 project_not_found` before `create_project`; `400 invalid_request` with `field: "overlap"` for anything outside `"0s"` to `"7d"`. It needs the `project` scope, like `create_project`.
- **Retries:** default 5 attempts total, max 5. Delays before retries 1 to 4 are 10 s, 2 min, 20 min and 40 min, with equal jitter, so five attempts span 31 to 62 minutes and outlast a 30-minute outage. A `Retry-After` from the destination is honoured as a minimum, up to 40 min. A task that keeps failing holds its lane, and delays its callback, a cron's `failing` alert or a failed wake's count, for that whole span; pass a lower `max_attempts` where speed matters more.
- **Timeout:** 10 s per attempt. Redirects are not followed.
- **Success:** any 2xx. Retry on 408, 425, 429, 5xx, timeouts and network errors. Anything else (other 4xx, 3xx) fails without retry.
- **Admission:** work is refused up front if the project has expired (`403 trial_expired`), holds too many tasks (`429 task_limit`), or would need more attempts than are left once waiting tasks are counted (`402 budget_exhausted`). A waiting task reserves one attempt, plus one for a callback to a URL and any for its `on_match` action. A task already being delivered reserves only those follow-ups (its attempt was charged when it started), and a pending wake reserves nothing.
- **Redrive:** `redrive_task` gives a failed task a fresh set of attempts under the same id; any other status is `409 task_not_failed`. A watch and a wake cannot be redriven (`409 watch_not_redrivable`, `wake_not_redrivable`), and neither can a task made through the A2A interface (`409 a2a_task_not_redrivable`, from every interface): A2A lets no finished task start again, so its request is scheduled again as a new task.
- **Idempotency:** an optional `idempotency_key` makes `schedule_http` and `queue_http` safe to repeat. The same key with the same request returns the original task; with a different request it is a conflict.
- **Queues:** `queue_http`'s `queue` and `schedule_http`'s `lane` are the same thing; a task shows it as `lane`, and `schedule_http` without a time still sends now. Tasks given the same one (1–64 of `A-Z a-z 0-9 _ - . :`) run one at a time. A task that has started (delivering, or waiting to retry) holds its lane until it succeeds, fails or is canceled; then the waiting task with the earliest due time (ties: earliest scheduled) goes next. A failed or canceled task does not block the lane. A redriven task rejoins it with a due time of now. Order is by due time, not by when tasks were scheduled: schedule A with `delay: "1h"` and then B for now in the same lane, and B runs first. Lanes order delivery attempts, not what the destination does with them; at-least-once still applies. Callbacks are not in any lane.
- **Delay:** `delay` is a relative alternative to `run_at_ms`: a whole number followed by `s`, `m`, `h` or `d` (`"90s"`, `"24h"`, `"7d"`), at most 7 days. Giving both is an error.
- **Callbacks:** with `callback_url`, every time the task finishes (succeeded, failed or canceled) AgentLater queues a *callback task*: a signed `POST` of `{"event": "task.finished", "task": {…}}` (the task as `get_task` returns it) to that URL. It follows the same destination rules, retries and signing, and is visible with `get_task` (`callback_task_id` on the task, `callback_for` on the callback). Each callback try costs one delivery attempt, so scheduling a task with a callback reserves two attempts and two task slots. Callbacks do not themselves call back. A callback always gets at least one try, even after the trial's attempts run out or it expires (but no retries then, and `max_attempts` shows 1); so `attempts_used` can go over the budget by one attempt per such callback. The report includes the task's full `url`, query string included, so don't put secrets there if the callback goes to another party. Callback tasks appear in `list_tasks` like any other.

## Inbox and captured responses

Callbacks need a public HTTPS URL, and most agents (everything on the hosted `/mcp`) have none. The inbox is where results wait for the agent's next session instead. Why and what was decided: [INBOX.md](INBOX.md).

- **Sending to it.** `callback_url: "inbox"` on `schedule_http` or `watch_http`, `alert_url: "inbox"` on `create_cron`: exactly that string; anything else is a URL under the destination rules. The payload is what the callback or alert would have POSTed (`task.finished` with the task, `cron.failing` / `cron.recovered`), written as an event in the same ledger transaction as the finish. It is not a task: no delivery attempt, no task slot, no retries, and it is written even past the budget or the trial. A task shows its latest event as `inbox_seq`; every finish adds one, so a redriven task reports again.
- **Reading it.** One inbox per project, an append-only log numbered by `seq` (1, 2, … never reused), with one cursor, `acked_seq`, kept by AgentLater because the agent can't keep it across sessions. `read_inbox` returns events after `after` (default `acked_seq`), oldest first, as `{seq, at_ms, event, …}`; `limit` is 1–50, default 10, since every event lands in the agent's context. Reading does not move the cursor: `ack_inbox(up_to)` does, or `read_inbox(ack: true)` up to the last event returned (with `ack`, an `after` past the cursor is refused, since it would ack the events skipped over). So delivery to the agent is at-least-once, like delivery to a destination; events carry the task id to dedupe on.
- **The cursor only moves forward.** An `up_to` at or below `acked_seq` changes nothing; one above the newest event is `400 ack_beyond_inbox` (it would ack events the agent hasn't read). Two sessions at once may both see an event; neither can move the other back.
- **Retention.** Unread events are kept 30 days, acked ones 7 days after they arrive (replayable with `after`). At 10,000 events a new one removes the oldest acked event, or, if none is acked, the oldest unread one. `dropped` counts unread events removed by age or by the cap since the cursor last moved; the reply that moves it still shows the count.
- **Capture.** `capture: true` on `schedule_http` keeps the response of the latest attempt that got one, success or failure, as the task's `response`: `attempt`, `status`, the `content-type`, `location`, `etag` and `retry-after` headers, the first 16 KiB of the body as UTF-8 (invalid bytes replaced), `body_bytes` from `Content-Length` (null for an encoded body), and `truncated`. Any HTTP answer replaces the earlier capture; a timeout or network error leaves it in place; a redrive keeps it until a new attempt replaces it. It shows in `get_task` and in the callback or inbox report; `list_tasks` leaves out the body. It costs nothing extra. It is deleted 7 days after the task finishes, and an inbox event's copy loses its body at the same time (its status and headers stay). A failed URL callback keeps its payload, never returned by the API, for redrive until it is purged. Reading the body never changes an attempt's outcome: if the body is slow or broken, the status and headers are recorded with `body: null` and `truncated: true`.
- **The response is untrusted.** It comes from whatever server the agent called. Tool descriptions tell agents to treat it as data, never as instructions.
- **Telling agents to look.** The hosted `/mcp` server's instructions, the `get_project` description and the plugin skill say: at the start of a session, if `get_project` shows `inbox_unread`, call `read_inbox`. `/llms.txt` only says to read the inbox in the next session.

## Wake

The inbox holds results until a session reads them; a wake starts that session. `set_wake` stores one URL per project, something that starts an agent run (a routine or workflow trigger, a chat webhook, the agent's own server), with headers such as its `Authorization`.

- **When.** After an inbox event from a task or a cron alert, if the wake is active and none is already waiting or in flight, AgentLater queues one for now, or for `debounce` (1m–24h, default 15m) after the last wake actually sent, if that was more recent. So a burst of results starts the agent once, and re-setting the wake never pushes a pending one later. A wake that comes due with nothing unread is canceled without an attempt and does not count as sent. A delivered wake covers every event up to when it was sent; events after that get the next wake, even if the setting was replaced meanwhile. A failed wake told the agent nothing, so anything still unread gets another.
- **What.** A signed `POST` of `{"event": "inbox.unread", "project_id", "unread"}`, the count when it is sent, and nothing else: results and captured responses stay behind the authenticated inbox. It is a task with `wake_call: true`, retried like any delivery and costing one attempt per try. It is not admitted, so it never spends attempts promised to waiting tasks: it goes only while an attempt is spare. With nothing else waiting it gets one try even past the budget or the trial; with work waiting and no spare attempt it is canceled (`budget_reserved`), which is not a wake failure. It can exceed the task cap by one at a time. A wake cannot be redriven (`409 wake_not_redrivable`), and its header values are wiped once it ends.
- **Turning off.** A wake that fails (all its attempts) counts; a delivered one resets the count. After 3 in a row the wake turns itself off (`state: disabled`, `disabled_reason: wake_failing`) and adds a `wake.disabled` event to the inbox, which does not itself wake. `set_wake` again replaces the URL, headers and debounce, turns it back on, cancels a wake not yet sent, and queues one if anything is unread. `delete_wake` removes it (404 `wake_not_set` if there is none); a wake already being sent finishes.
- `get_project` shows `wake`: URL, header names (never values), debounce, state, failures in a row, and the last wake's time and task id.

## Secrets

Agents handle untrusted text (pages, documents, captured responses, hook requests), so a credential pasted into a request can be stolen by an instruction that redirects it. A secret is stored once and referred to by name, and it can only be sent to the hosts it was set for.

- **Setting.** `set_secret(name, value, allowed_hosts)`: `name` is `A-Z0-9_`, starting with a letter, at most 64; `value` is 16–4096 bytes of printable ASCII with no line breaks (it goes in a header; a shorter one could be guessed by reflecting candidates off an allowed host and watching which come back redacted); `allowed_hosts` is 1–20 exact public host names (`api.github.com`). There are no wildcards: under shared domains (`workers.dev`, `amazonaws.com`, `github.io`) anyone can get a name, and no list reliably says which domains those are. Same name again replaces value and hosts. At most 50 per project (`429 secret_limit`).
- **Write-only.** The API seals the value (AES-256-GCM, a key per project derived with HKDF from the `SECRETS_KEY` Worker secret, the name as associated data) before the ledger stores it; the ledger never holds plaintext. No route, view, callback, inbox event or log returns a value: `list_secrets` (scope `read`) shows names, hosts and times. Setting and deleting need the `secrets` scope.
- **Using.** `{{secret:NAME}}` inside a header value of `schedule_http`, `create_cron`, `watch_http` or `set_wake`. Anywhere else (URL, body) it is an error rather than text sent as is. The task stores the reference; when work is accepted, every name must exist (`400 secret_not_found`), allow the destination's host (`400 secret_host_not_allowed`, field `url`), and the headers must fit 8 KiB with each value's real length counted.
- **Sending.** At each attempt the ledger checks again and hands the consumer the sealed values; the consumer opens them, fills the headers in, sends, and forgets them. A secret deleted, narrowed or replaced by one too long in the meantime fails the task at once, without an attempt, with `failure: secret_missing`, `secret_host_not_allowed` or `secret_too_large`; an API without its key fails it with `secret_unavailable` (each reports like any failure, a watch shows it as `watch.reason`, and a redrive after fixing it sends it). Rotation is immediate: the next attempt of anything referring to the name uses the new value.
- **Echoes.** Before a captured response (or a watch's last body) is stored, every value the attempt used is replaced with `[secret:NAME]`, as sent, JSON-escaped or percent-encoded, in the body and the kept headers. The body is read past its cap by up to 4 KiB first, so a value straddling the cap is redacted whole, then cut. This is best effort: a destination that echoes a value base64-encoded or masked is not caught, so bind secrets only to hosts that don't reflect request headers.
- **Who can change them.** Any caller with the `secrets` scope (every agent of the project) can replace or delete a project's secrets. Replacing keeps the value behind the same hosts, so the worst case is requests going to the right host with the wrong credential.
- **Key.** One `SECRETS_KEY` per environment (`scripts/setup-deploy-secrets.mjs`). Without it the API runs and `set_secret` answers `503 secrets_unavailable`. It can be rotated without failing anyone's work: the old key, set as `SECRETS_KEY_PREVIOUS`, keeps opening values while a sweep reseals them under the new one (`docs/adr/0001-rotating-secrets-key.md`, and `docs/DEPLOY.md` for the steps).

## Inbound hooks

A hook lets an agent with no server receive webhooks: GitHub, Stripe, a CI system, anything that POSTs when something happens. Why and what was decided: [HOOKS.md](HOOKS.md).

- **Creating.** `create_hook(name, verify?, keep_headers?, accept?)` returns `url`, `https://hooks.agentlater.io/h/{project_id}/hk_<64 hex>`, **this once**: the ledger stores only the token's SHA-256. Creating again with the same name gives a new URL; the old one answers 404 at once, and the counts start again. `name` follows the cron name rules. At most 20 per project (`429 hook_limit`); not after the trial ends (`403 trial_expired`).
- **Verifying.** `verify: { scheme, secret }`: `github` (`X-Hub-Signature-256: sha256=<hex>` of the body), `stripe` (`Stripe-Signature: t=…,v1=…` over `"{t}.{body}"`; any `v1` may match; `t` within 5 minutes, else `401 signature_stale`), or `hmac_sha256` with `header` (required) and optional `prefix`, hex or base64. `secret` is 16–1024 printable ASCII bytes, sealed like a project secret (under `hook:{name}`, so it can't be opened as one) and never shown. It is not taken from `set_secret`: those values may only be *sent* to their hosts. A bad, missing or stale signature is `401` and writes nothing to the inbox (it counts in the hook's `refused`). Without `verify`, any POST to the URL is accepted and its event says `verified: false`.
- **What an event keeps.** `hook.received` with `hook`, `verified`, `delivery_id`, and `request`: `method`, `headers`, the first 16 KiB of the body as text, `body_bytes` and `truncated`. Headers kept: `content-type`, `user-agent`, the scheme's event headers (`x-github-event`, `x-github-delivery`, `x-github-hook-id`), and up to 10 `keep_headers`. Never `authorization`, `cookie`, `proxy-authorization`, `set-cookie`, `x-api-key`, `x-auth-token`, `x-access-token`, `x-gitlab-token` or the signature; asking to keep them is a `400`. Bodies are kept as text: bytes that aren't UTF-8 become U+FFFD. The body is deleted from the event 7 days after it arrives, like a captured response. Hook events wake the agent like any inbox event.
- **Filtering.** `accept` (on `create_hook` or `update_hook`) is 1–5 rules; a request is kept if any matches. A rule has `headers` (1–5 names, case-insensitive, each with 1–20 exact values of up to 256 bytes, without surrounding spaces) and/or `path` + `in` (as a watch's `until`, against the whole body read as JSON); every part given must hold, and an empty rule is a `400`. Headers are checked first and the body is parsed at most once. A request that matches none is answered `202` like a kept one, so the sender doesn't retry, but writes nothing, wakes nothing, and uses neither a daily cap nor the hook's rate limit (so noise can't crowd out a match); it counts in `filtered`. The filter runs after the signature check (a forged request is still `401`) and after the rate-limit check (a flood over it is refused before any matching), and before delivery-id dedupe, so a filtered request records no id. Filters cut noise; on an unverified hook they are no protection (anyone with the URL can send what matches, and once a cap is reached the different answers reveal the filter). Design: [HOOK_FILTERS.md](HOOK_FILTERS.md).
- **Changing a hook.** `update_hook(name, verify?, accept?, keep_headers?)` keeps the URL, the counts and the delivery ids seen. Fields left out stay; `accept: null` keeps everything again, `verify: null` stops checking signatures, `keep_headers: null` is the same as `[]`; a new `verify` replaces the secret. `keep_headers` is checked against whichever `verify` then applies.
- **Retries.** A verified sender's delivery id (`X-GitHub-Delivery`; a Stripe event's `id`) seen on the same hook in the last 24 hours gets `200` and is not written again. Unsigned requests have no trusted id, so a retry is a second event.
- **Answers.** `202` written, `200` retry, `401` signature, `404` unknown hook or project, `405` anything but POST (GET and HEAD included, so a URL can't be probed), `410` the project's trial has ended, `413` over 256 KiB, `429` rate limited (`Retry-After`), `503` AgentLater can't check signatures right now. The body is always `{}`: nothing the sender wrote goes back.
- **Limits and cost.** No delivery attempts: receiving is an inbox write. Per hook, 60 requests a minute in bursts of 20; per project, 1,000 events per UTC day, 10,000 once a pack is bought; per hook without `verify`, 100 a day, so one leaked URL can't use up the project's day (`429 daily_limit`, `Retry-After` until midnight UTC). A full inbox drops its oldest hook event first and never an unread task report for one: with nothing else to drop, the request is `429 inbox_full`. In front, the gateway limits each client IP to 120 and each project to 600 requests a minute. `list_hooks` counts what each hook `received` and `refused`, by reason. GitHub's delivery id isn't signed and its scheme has no timestamp, so a captured request can be replayed; treat events as untrusted.
- **Where it runs.** `hooks.agentlater.io` (`hooks-staging.agentlater.io` on staging) is served by the gateway and serves nothing else. The gateway answers 404 for a project it doesn't know (so a made-up id never creates a ledger), reads at most 256 KiB, and hands the request to the project's ledger with a `hooks.receive` assertion, a scope no agent is granted.

## Crons

A cron runs the same HTTP request on a schedule: five fields (`minute hour day-of-month month day-of-week`, with `*`, numbers, `a-b`, `*/n`, `a-b/n` and lists), evaluated in **UTC**, at most 128 characters. When both day fields are restricted, a day matches if either does; if either starts with `*` (e.g. `*/2`), both must match, as in Vixie cron.

- **Runs are tasks.** At each slot the ledger starts a task (`cron` names its cron) with the cron's request and `max_attempts`; `get_task` shows it. Runs are admitted like any task, so each costs delivery attempts. A run that cannot start (no attempts left, task cap reached) is a failure, not a silent skip.
- **Missed slots are not made up.** If the scheduler wakes late, the cron runs once and moves to the next slot after now.
- **At most once an hour.** The minute field must be a single value; anything that could run twice in an hour is refused. The trial's 1,000 attempts outlast its 7 days at hourly (168 runs) as long as runs succeed first time.
- **Dead man's switch.** `health` is `pending` until a run finishes, `healthy` after a success, `failing` after a failed run or a run that could not start (`health_reason`: `run_failed`, `budget_exhausted`, `task_limit`, `trial_expired`). Becoming failing sends one signed alert to `alert_url` (`{"event": "cron.failing", "reason", "task_id", "cron"}`); the next success sends `cron.recovered`. Alerts are tasks like callbacks (`alert_for` names the cron) and get one try even past the trial; they are not admitted, so they can exceed the task cap by one per health change. The alert includes the cron's full `url`, query string included. A run that started but could not be attempted (attempts ran out between start and send) reports `run_failed`. Only runs of the current cron count, and a run older than the one that last set the health is ignored.
- **Runs can report.** `callback_url` (a URL or `"inbox"`) and `capture` work on each run exactly as on a task scheduled with them: the run's `task.finished` report goes there, with its captured response, and an inbox report can wake the agent. `report: "failures"` (only with `callback_url`; the default is `"always"`) holds back the report of a run that succeeds or is canceled, so a healthy cron is quiet; such runs show `report_failures_only`. A run abandoned for lack of budget has failed, so it reports. A run that cannot start at all (`budget_exhausted`, `task_limit`, `trial_expired`) is not a task and reports nothing: it turns the cron failing, which reaches `alert_url`, so a cron meant to be watched sets both. A run reporting to a URL is admitted with a second attempt and task slot for its callback task, even with `failures` where most runs won't use it, so a project with one attempt left marks the cron failing (`budget_exhausted`) instead. Alerts carry the cron's `callback_url`, query string included. Replacing a cron by name replaces these too; runs already started keep what they started with.
- **The trial ending stops crons:** `next_run_ms` becomes null and they alert with `trial_expired`. Buying a pack starts them again.
- Re-creating a cron by name replaces its schedule and request and keeps its health. Deleting one stops it; a run already started finishes but no longer affects health.

## Watches

A watch hands off waiting on someone else's long-running job: AgentLater polls a status URL until the job is done, then reports back.

- **A watch is a task.** Its attempts are polls (`GET` by default), signed like any delivery. The first poll is immediate; after one that is not done, the next runs `every` later (5m–24h). `get_task` shows progress under `watch`, `cancel_task` stops it, and `callback_url` gets the final state, as with any task.
- **Done means `until` matches.** `path` + `in`: a 2xx response whose JSON body has one of the listed values (strings, numbers, booleans, null) at that JSON Pointer, e.g. `{"path": "/status", "in": ["succeeded", "failed"]}`. `status`: the response status is one of the listed codes, e.g. `{"status": [404]}` to wait until something is gone. Given both, both must hold. Only the first 64 KiB of a response is read. Numbers compare by value (`3` matches `3.0`). No expressions or regexes.
- **How it ends** (`watch.reason`): `matched` (succeeded; `watch.matched` is the value), `timed_out` (the last poll is at the deadline itself), `poll_failing` (3 polls in a row got a 408, 425, 429, 5xx, timeout or network error), `poll_rejected` (any other non-matching response that is not 2xx, e.g. 401, 404 or a redirect; it will not change by waiting), `response_too_large` (`until` has a `path` and the matching response's body is over 64 KiB, so it can never be read), `budget_exhausted` (the trial ran out of attempts), or a secret failure (`secret_missing`, `secret_host_not_allowed`, `secret_too_large`, `secret_unavailable`; see Secrets). All but `matched` are failures. A canceled watch has no reason.
- **Each poll costs one delivery attempt.** Admission reserves one, like any task; the rest come from the trial as polls run, so a long watch can use up attempts that tasks already scheduled were counting on (they then fail with `budget_exhausted`). Polling every 5 minutes for 6 hours is 73 attempts, most of the trial.
- **What is kept:** `watch.last_status` and up to 16 KiB of the last response body (`watch.last_body`, in `get_task` and the callback but not in `list_tasks`). Header values and the body are deleted when the watch ends, since they often hold an API token; header names stay.
- `deadline` is at most 7 days and must fall before the trial ends. A watch cannot be redriven (`409 watch_not_redrivable`); start a new one.

### Acting on a match

`on_match` lets a watch take the next step itself, without waiting for the agent's next turn: when the deploy reports `SUCCEEDED`, start the smoke tests.

```json
{
  "url": "https://deploy.example.com/deployments/123", "every": "5m", "deadline": "6h",
  "until": { "path": "/status", "in": ["SUCCEEDED"] },
  "on_match": {
    "url": "https://ci.example.com/smoke-tests",
    "headers": { "Authorization": "Bearer {{secret:CI_TOKEN}}" },
    "body": "{\"deploymentId\":\"123\"}",
    "callback_url": "inbox"
  },
  "callback_url": "inbox"
}
```

- **What it is.** One request, given when the watch is created: `schedule_http`'s `url`, `method` (POST by default), `headers`, `body`, `max_attempts`, `callback_url` and `capture`. No delay, lane or idempotency key (it goes as soon as the watch matches), and it cannot be a watch, so one watch sets off at most one request and nothing chains. AgentLater never makes up a request at match time.
- **Checked up front.** It is validated with the watch (`400`, field `on_match`, message naming the inner field). A secret it refers to must exist and allow the action's own host; allowing the watched host is not enough.
- **When it is sent.** Only on `matched`. A timeout, `poll_failing`, `poll_rejected`, `budget_exhausted` or a cancel sends nothing. A cancel asked for while the matching poll was out still records the match, but sends nothing (`watch.action_skipped: "canceled"`).
- **Once.** The action is queued in the same ledger transaction that records the match, under an id derived from the watch's. Only the poll holding the watch's lease can finish it, so a late or duplicate poll result changes nothing, and queueing the same action twice is a no-op. After that it is an ordinary task: signed, retried, delivered **at least once**, so the receiver should dedupe on `RunLater-Task-Id`.
- **Budget.** Admission reserves the action's attempt (and its callback's, for a URL) and task slots when the watch is created, alongside the watch's own. A project that cannot afford them gets `402` then, not hours later. Its first attempt is guaranteed, even past the trial, as a callback's is; retries come from the normal budget.
- **Linking.** The watch shows `watch.on_match` (method, URL, header names, `callback_url`, `capture`) and, once it matched, `watch.action_task_id`; the watch's own report carries both. The action shows `triggered_by`. The watch and its action report separately, each to its own `callback_url`.
- **What is kept.** As with the watch, the action's header values and body are deleted from the watch when it ends; the action task keeps its own until it finishes, like any task.

## Buying attempts

An agent cannot pay, so a person pays for it. Stripe takes the payment; AgentLater never sees card details.

1. The agent calls `get_payment_link` and passes the `payment_url` to its person. When attempts run out or expire, the `budget_exhausted` / `trial_expired` error says so.
2. The person opens `agentlater.io/pay#…` and pays with Stripe's Embedded Checkout (Stripe.js). The link opens one Stripe Checkout Session and works for 24 hours. The publishable key and the session's client secret travel in the URL fragment, so they never reach a server or a `Referer`.
3. Stripe calls the gateway's webhook. The pack is added to the project the link was made for, once per Checkout Session however often Stripe retries.

**Or the agent pays itself, over MPP** ([Machine Payments Protocol](https://mpp.dev)), if it holds a payment credential (for example a Shared Payment Token from Stripe's `link-cli`):

1. `get_payment_link` also returns `mpp_url`, signed for this project and valid 7 days.
2. `POST mpp_url` answers `402` with a Stripe charge challenge for one pack. The agent retries with its credential; AgentLater charges it through a Stripe PaymentIntent and adds the pack before answering `200` with a `Payment-Receipt`.
3. If adding fails after the charge, the answer is `202` and Stripe's `payment_intent.succeeded` webhook adds the pack instead. A credential works once; each PaymentIntent is applied once.
- MPP takes cards only for now (Shared Payment Tokens); stablecoins (Tempo) could follow.

- **One pack:** 50,000 delivery attempts for $5.00 (10¢ per 1,000). Card fees are about 9% of that; Cloudflare costs about $0.03 per 1,000 attempts.
- **Validity:** each purchase makes the project's whole balance valid for 12 months from that day. Attempts not yet expired, from the trial or an earlier pack, carry over; expired ones do not come back.
- **A purchase reopens a project:** a project whose trial or attempts expired takes new work again, and crons stopped by expiry start again from their next slot.
- **Paid projects hold more:** up to 20,000 tasks instead of 1,000.
- Other limits are unchanged; `run_at_ms`, watch deadlines and cron runs still cannot fall after the project's expiry.
- Billing is off until Stripe keys are configured (`billing_unavailable`).

## v1 limits

| Limit | Value |
| --- | --- |
| Request body | 64 KB |
| Headers | 32 headers, 8 KB total |
| Furthest `run_at_ms` | 7 days out (bounded by trial expiry) |
| Trial | 1,000 delivery attempts, 7 days |
| New trials | 100 a day across the service (`NEW_TRIALS_PER_DAY`), counted over the last 24 hours; beyond that `create_project` answers `429 trials_paused` for new owners |
| Attempt pack | 50,000 delivery attempts for $5; balance valid 12 months from the latest purchase |
| Retention | Task + attempts kept 30 days after terminal state |
| Tasks held per project | 1,000 (20,000 once a pack is bought), finished ones included until purged. A task's body is dropped once it succeeds or is canceled; failed tasks keep it for redrive. |
| Projects | 1 per owner (Agent Auth host or hosted MCP connection) |
| Signing secret rotation | Overlap from 0 s to 7 days (default 24 h); at most two signing secrets sign at a time |
| Crons | 10 per project, at most one run an hour each |
| Watches | One poll every 5 min to 24 h, for at most 7 days; first 64 KB of a response read |
| Captured response | First 16 KiB and 4 headers; deleted 7 days after the task finishes |
| Inbox | 10,000 events per project; unread kept 30 days, acked 7 days; `read_inbox` returns at most 50 |
| Wake | One per project; at most one wake per 1m–24h (default 15m); off after 3 failures in a row |
| Secrets | 50 per project; 4 KiB each; 1–20 allowed hosts each; header values only |
| Hooks | 20 per project; 256 KiB per request (16 KiB kept); 60 requests a minute per hook (bursts of 20); 1,000 events a day per project (10,000 once a pack is bought); 100 a day per hook without `verify` |

## Explicitly not in v1

| Out of scope | Why it's later |
| --- | --- |
| Web dashboard | A page where a human signs in to see tasks. v1 has no humans signing in. |
| Subscriptions, invoices, larger packs | One pack size, paid by card, covers launch. |
| SDKs | A packaged npm client. Agents use capabilities/MCP directly. |
| API keys, invitations, receiver pairing | Replaced by Agent Auth and hosted MCP's one-click OAuth. |
| Workflows, fan-out, ordering across lanes | Different product. Lanes give one-at-a-time order within a lane only. `on_match` is one request and doesn't chain. |
| Hooks that send requests | Anyone with a hook's URL could spend the project's attempts. It needs `verify` to be required and its own spec. |
| Synchronous hook challenges (Slack `url_verification`, Meta `hub.challenge`) | They need AgentLater to echo sender input, per scheme. |
| Notes an agent writes to its own future inbox | AgentLater as timed memory is a product decision of its own. |

## Success criteria for v1 launch

- An agent goes from zero to a delivered, signed request in under 5 tool calls, with at most one human click.
- Accepted tasks are never lost: every one reaches `succeeded`, `failed` or `canceled`.
- No task can reach a private network or another project's data (tested).
- p95 delivery lateness under 60 s for tasks due "now", measured on a real deployment. Measured from the first attempt of each task at its original time (`scripts/stats.mjs`); retries, redrives, lanes and wakes are late by design and are left out.

Hosted MCP needs that one click, on the consent page, then `create_project` and `schedule_http`. Agent Auth needs no human at all.

## Open questions

1. ~~**Remote-only MCP clients.**~~ Resolved: a hosted `/mcp` with one-click OAuth consent (no account), one project per connection.
2. ~~**Key loss.**~~ Resolved: the project is gone with its keys, inbox included, and is never moved to a new owner: paying for a project doesn't prove owning it, and payment links are meant to be passed around. The unused part of its purchased attempts may be refunded to the original payment method by hand, at our discretion ([DEPLOY.md](../DEPLOY.md#lost-keys-refund-the-unused-attempts)).
3. **Abuse.** New keys are new identities (Sybil). Today there are per-IP limits on registration (10 a minute) and on OAuth client registration and consent (20 a minute each), and a global cap on new trials (`NEW_TRIALS_PER_DAY`). The cap bounds the cost of free identities at about $3 a day at $0.03 per 1,000 attempts, but one abuser can use it up and block real sign-ups. Is that enough while the trial is free?
4. ~~**What happens after the trial?**~~ Resolved: the agent's person buys attempt packs (*Buying attempts*).
5. ~~**Rate of calls on `/mcp`.**~~ Resolved: 300 tool calls a minute per owner, over MCP and Agent Auth alike (`429 rate_limited`). Budgets bound what costs attempts; this bounds reads and refused calls, well above any agent's normal pace.
6. ~~**Measuring lateness.**~~ Resolved: a metric recorded with each attempt, in Workers Analytics Engine, separating real use from the monitor's journey.

## The other specs

| Spec | Covers |
| --- | --- |
| [ARCHITECTURE.md](ARCHITECTURE.md) | The Workers, where data lives, trust boundaries, testing and deploys |
| [DESIGN.md](DESIGN.md) | Design for agents (tools, errors, defaults) and for people (the site, consent and pay pages) |
| [INBOX.md](INBOX.md) · [HOOKS.md](HOOKS.md) · [HOOK_FILTERS.md](HOOK_FILTERS.md) | Why each of those features is the way it is. This file holds the rules; when they disagree, this file wins. |
