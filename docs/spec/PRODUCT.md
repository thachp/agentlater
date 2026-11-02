# RunLater — Product Spec (v1)

Status: **v1, live** at `gateway.runlater.io` · Owner: Patrick Thach · Last updated: 2026-11-02

## One line

**RunLater lets an AI agent say "make this HTTP request later" — and then check whether it happened.**

## Problem

Agents work in short sessions. They cannot sleep for an hour, wake up tomorrow, or
retry a flaky webhook five times over twenty minutes. Today an agent that needs
"call this URL at 9am" or "retry this until it succeeds" has nowhere to put that
intent. Developers solve it with cron boxes, queues, or QStash-style services that
assume a human signed up, pasted an API key, and wired a dashboard.

## Who it's for

| Priority | User | How they reach us |
| --- | --- | --- |
| Primary | **AI agents** acting for a person or system | Agent Auth capabilities, or the same capabilities as tools on the hosted MCP server (`/mcp`) |
| Secondary | Developers building those agents | Same capabilities over HTTP; docs site |

There is no human sign-up, password, or dashboard in v1. The agent *is* the customer.

## Core jobs (v1)

1. **Schedule** an HTTP request to a public HTTPS URL, now or at a future time.
2. **Check** a task's status and its delivery attempts.
3. **List** recent tasks for the project.
4. **Cancel** a task that hasn't run yet.
5. **Redrive** a task that failed, to try it again.

That's the whole product for v1.

## How it works (user's view)

```
Agent ──discover──▶ RunLater           "what can you do?"
Agent ──register──▶ Agent Auth         gets an identity + keys
Agent ──create_project──▶ RunLater     gets a free trial (finite budget, 7 days)
Agent ──schedule_http──▶ RunLater      { url, method, headers, body, run_at }
                                       ← task_id, status: scheduled
          … time passes …
RunLater ──signed HTTPS request──▶ destination   (retries on failure)
Agent ──get_task──▶ RunLater           ← status: succeeded, attempts: [...]
```

## Concepts

| Term | Meaning |
| --- | --- |
| **Agent identity** | An Agent Auth host + agent keypair. The only way to authenticate. |
| **Project** | Owned by exactly one agent host. Holds tasks, budget, and the signing secret. One project per host in v1. |
| **Trial** | A project's free allocation: N delivery attempts, expires after 7 days. No refills. |
| **Attempt pack** | Delivery attempts bought for a project: 50,000 for $5. See *Buying attempts*. |
| **Task** | One HTTP request to make at `run_at`, with a retry policy. |
| **Attempt** | One try at delivering a task: time, response status, duration, error. |

## Task lifecycle

```
scheduled ──(run_at reached)──▶ delivering ──2xx──▶ succeeded
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

- **Hosted MCP server** at `https://gateway.runlater.io/mcp` (Streamable HTTP). Each capability is a tool. Clients sign in with standard MCP OAuth: on first connect a RunLater consent page opens, the person clicks **Connect**, and there is no account. Each approved connection is its own owner with its own project and trial, separate from any Agent Auth identity; the OAuth grant is its only key (refresh tokens last 30 days from last use).
- **Agent Auth MCP bridge**, which runs locally and exposes generic tools (`connect_agent`, `list_capabilities`, `execute_capability`); the agent picks a RunLater capability by name inside `execute_capability`:

```json
{ "mcpServers": { "runlater": {
  "command": "npx",
  "args": ["@auth/agent-cli@0.5.1", "mcp", "--url", "https://gateway.runlater.io"],
  "env": { "AGENT_AUTH_ENCRYPTION_KEY": "<32 random bytes, hex>" }
} } }
```

The bridge keeps the agent's keys in `~/.agent-auth`. Losing them loses access to the project.

| Capability | Input | Output |
| --- | --- | --- |
| `create_project` | — | project id, trial budget, expiry |
| `get_project` | — | budget used/remaining, expiry, `paid`, signing secret |
| `get_payment_link` | — | `payment_url` for a person to buy a pack, `mpp_url` for the agent to buy one over MPP, expiries, price |
| `schedule_http` | `url`, `method`, `headers?`, `body?`, `run_at?` or `delay?`, `idempotency_key?`, `max_attempts?`, `callback_url?`, `lane?` | `task_id`, `status` |
| `get_task` | `task_id` | task + attempts |
| `list_tasks` | `status?`, `lane?`, `cursor?` | page of tasks |
| `cancel_task` | `task_id` | task |
| `redrive_task` | `task_id` | task |
| `create_cron` | `name`, `schedule`, `url`, `method?`, `headers?`, `body?`, `max_attempts?`, `alert_url?` | cron |
| `get_cron` | `name` | cron, with health |
| `list_crons` | — | crons |
| `delete_cron` | `name` | cron as it was |
| `watch_http` | `url`, `every`, `until`, `deadline`, `method?`, `headers?`, `body?`, `callback_url?`, `idempotency_key?` | task, with `watch` |

The REST routes underneath (`/v1/tasks`, …) are an internal contract between
services, not a public product in v1.

## Delivery rules

- **Destinations:** `https://` on the default port to a public DNS name. No IP literals, credentials in the URL, fragments, or non-public names (`localhost`, `*.local`, `*.internal`, `*.test`, …). Checked when the task is accepted and again before every attempt. Resolved addresses are restricted to the public internet by Cloudflare (`global_fetch_strictly_public`).
- **Methods and headers:** `GET`, `POST` (default), `PUT`, `PATCH`, `DELETE`. Callers may set any header except hop-by-hop/framing headers (`Host`, `Content-Length`, `Transfer-Encoding`, …) and the `RunLater-*`, `CF-*`, `X-Forwarded-*` and `Proxy-*` families. `GET` cannot have a body.
- **Signing:** every outbound request carries `RunLater-Task-Id`, `RunLater-Attempt`, `RunLater-Timestamp` (unix seconds) and `RunLater-Signature: v1=<hex>`, an HMAC-SHA256 with the project's signing secret over `"{timestamp}.{task_id}.{attempt}."` followed by the raw body. While a secret is being rotated, the header carries one comma-separated `v1=` entry per active secret. Receivers should reject timestamps more than 5 minutes off.
- **Retries:** default 5 attempts total, max 5. Delay before retry *n* is 10 s × 2ⁿ⁻¹ (capped at 10 min) with equal jitter; a `Retry-After` from the destination is honoured as a minimum, up to the same cap.
- **Timeout:** 10 s per attempt. Redirects are not followed.
- **Success:** any 2xx. Retry on 408, 425, 429, 5xx, timeouts and network errors. Anything else (other 4xx, 3xx) fails without retry.
- **Idempotency:** an optional `idempotency_key` makes `schedule_http` safe to repeat. The same key with the same request returns the original task; with a different request it is a conflict.
- **Lanes:** tasks given the same `lane` (1–64 of `A-Z a-z 0-9 _ - . :`) run one at a time. A task that has started (delivering, or waiting to retry) holds its lane until it succeeds, fails or is canceled; then the waiting task with the earliest due time (ties: earliest scheduled) goes next. A failed or canceled task does not block the lane. A redriven task rejoins it with a due time of now. Order is by due time, not by when tasks were scheduled: schedule A with `delay: "1h"` and then B for now in the same lane, and B runs first. Lanes order delivery attempts, not what the destination does with them; at-least-once still applies. Callbacks are not in any lane.
- **Delay:** `delay` is a relative alternative to `run_at_ms`: a whole number followed by `s`, `m`, `h` or `d` (`"90s"`, `"24h"`, `"7d"`), at most 7 days. Giving both is an error.
- **Callbacks:** with `callback_url`, every time the task finishes (succeeded, failed or canceled) RunLater queues a *callback task*: a signed `POST` of `{"event": "task.finished", "task": {…}}` (the task as `get_task` returns it) to that URL. It follows the same destination rules, retries and signing, and is visible with `get_task` (`callback_task_id` on the task, `callback_for` on the callback). Each callback try costs one delivery attempt, so scheduling a task with a callback reserves two attempts and two task slots. Callbacks do not themselves call back. A callback always gets at least one try, even after the trial's attempts run out or it expires (but no retries then, and `max_attempts` shows 1); so `attempts_used` can go over the budget by one attempt per such callback. The report includes the task's full `url`, query string included, so don't put secrets there if the callback goes to another party. Callback tasks appear in `list_tasks` like any other.

## Crons

A cron runs the same HTTP request on a schedule: five fields (`minute hour day-of-month month day-of-week`, with `*`, numbers, `a-b`, `*/n`, `a-b/n` and lists), evaluated in **UTC**, at most 128 characters. When both day fields are restricted, a day matches if either does; if either starts with `*` (e.g. `*/2`), both must match, as in Vixie cron.

- **Runs are tasks.** At each slot the ledger starts a task (`cron` names its cron) with the cron's request and `max_attempts`; `get_task` shows it. Runs are admitted like any task, so each costs delivery attempts. A run that cannot start (no attempts left, task cap reached) is a failure, not a silent skip.
- **Missed slots are not made up.** If the scheduler wakes late, the cron runs once and moves to the next slot after now.
- **At most once an hour.** The minute field must be a single value; anything that could run twice in an hour is refused. The trial's 100 attempts last about four days at hourly, about three months at daily.
- **Dead man's switch.** `health` is `pending` until a run finishes, `healthy` after a success, `failing` after a failed run or a run that could not start (`health_reason`: `run_failed`, `budget_exhausted`, `task_limit`, `trial_expired`). Becoming failing sends one signed alert to `alert_url` (`{"event": "cron.failing", "reason", "task_id", "cron"}`); the next success sends `cron.recovered`. Alerts are tasks like callbacks (`alert_for` names the cron) and get one try even past the trial; they are not admitted, so they can exceed the task cap by one per health change. The alert includes the cron's full `url`, query string included. A run that started but could not be attempted (attempts ran out between start and send) reports `run_failed`. Only runs of the current cron count, and a run older than the one that last set the health is ignored.
- **The trial ending stops crons:** `next_run_ms` becomes null and they alert with `trial_expired`. Buying a pack starts them again.
- Re-creating a cron by name replaces its schedule and request and keeps its health. Deleting one stops it; a run already started finishes but no longer affects health.

## Watches

A watch hands off waiting on someone else's long-running job: RunLater polls a status URL until the job is done, then reports back.

- **A watch is a task.** Its attempts are polls (`GET` by default), signed like any delivery. The first poll is immediate; after one that is not done, the next runs `every` later (5m–24h). `get_task` shows progress under `watch`, `cancel_task` stops it, and `callback_url` gets the final state, as with any task.
- **Done means `until` matches.** `path` + `in`: a 2xx response whose JSON body has one of the listed values (strings, numbers, booleans, null) at that JSON Pointer, e.g. `{"path": "/status", "in": ["succeeded", "failed"]}`. `status`: the response status is one of the listed codes, e.g. `{"status": [404]}` to wait until something is gone. Given both, both must hold. Only the first 64 KiB of a response is read. Numbers compare by value (`3` matches `3.0`). No expressions or regexes.
- **How it ends** (`watch.reason`): `matched` (succeeded; `watch.matched` is the value), `timed_out` (the last poll is at the deadline itself), `poll_failing` (3 polls in a row got a 408, 425, 429, 5xx, timeout or network error), `poll_rejected` (any other non-matching response that is not 2xx, e.g. 401, 404 or a redirect; it will not change by waiting), `response_too_large` (`until` has a `path` and the matching response's body is over 64 KiB, so it can never be read), `budget_exhausted` (the trial ran out of attempts). All but `matched` are failures. A canceled watch has no reason.
- **Each poll costs one delivery attempt.** Admission reserves one, like any task; the rest come from the trial as polls run, so a long watch can use up attempts that tasks already scheduled were counting on (they then fail with `budget_exhausted`). Polling every 5 minutes for 6 hours is 73 attempts, most of the trial.
- **What is kept:** `watch.last_status` and up to 16 KiB of the last response body (`watch.last_body`, in `get_task` and the callback but not in `list_tasks`). Header values and the body are deleted when the watch ends, since they often hold an API token; header names stay.
- `deadline` is at most 7 days and must fall before the trial ends. A watch cannot be redriven (`409 watch_not_redrivable`); start a new one.

## Buying attempts

An agent cannot pay, so a person pays for it. Stripe takes the payment; RunLater never sees card details.

1. The agent calls `get_payment_link` and passes the `payment_url` to its person. When attempts run out or expire, the `budget_exhausted` / `trial_expired` error says so.
2. The person opens `runlater.io/pay#…` and pays with Stripe's Embedded Checkout (Stripe.js). The link opens one Stripe Checkout Session and works for 24 hours. The publishable key and the session's client secret travel in the URL fragment, so they never reach a server or a `Referer`.
3. Stripe calls the gateway's webhook. The pack is added to the project the link was made for, once per Checkout Session however often Stripe retries.

**Or the agent pays itself, over MPP** ([Machine Payments Protocol](https://mpp.dev)), if it holds a payment credential (for example a Shared Payment Token from Stripe's `link-cli`):

1. `get_payment_link` also returns `mpp_url`, signed for this project and valid 7 days.
2. `POST mpp_url` answers `402` with a Stripe charge challenge for one pack. The agent retries with its credential; RunLater charges it through a Stripe PaymentIntent and adds the pack before answering `200` with a `Payment-Receipt`.
3. If adding fails after the charge, the answer is `202` and Stripe's `payment_intent.succeeded` webhook adds the pack instead. A credential works once; each PaymentIntent is applied once.
- MPP takes cards only for now (Shared Payment Tokens); stablecoins (Tempo) could follow.

- **One pack:** 50,000 delivery attempts for $5.00 (10¢ per 1,000). Card fees are about 9% of that; Cloudflare costs about $0.03 per 1,000 attempts.
- **Validity:** each purchase makes the project's whole balance valid for 12 months from that day. Attempts not yet expired, from the trial or an earlier pack, carry over; expired ones do not come back.
- **A purchase reopens a project:** a project whose trial or attempts expired takes new work again, and crons stopped by expiry start again from their next slot.
- **Paid projects hold more:** up to 20,000 tasks instead of 1,000.
- Other limits are unchanged; `run_at`, watch deadlines and cron runs still cannot fall after the project's expiry.
- Billing is off until Stripe keys are configured (`billing_unavailable`).

## v1 limits

| Limit | Value |
| --- | --- |
| Request body | 64 KB |
| Headers | 32 headers, 8 KB total |
| Furthest `run_at` | 7 days out (bounded by trial expiry) |
| Trial | 100 delivery attempts, 7 days |
| Attempt pack | 50,000 delivery attempts for $5; balance valid 12 months from the latest purchase |
| Retention | Task + attempts kept 30 days after terminal state |
| Tasks held per project | 1,000 (20,000 once a pack is bought), finished ones included until purged. A task's body is dropped once it succeeds or is canceled; failed tasks keep it for redrive. |
| Projects | 1 per agent host |
| Crons | 10 per project, at most one run an hour each |
| Watches | One poll every 5 min to 24 h, for at most 7 days; first 64 KB of a response read |

## Explicitly not in v1

| Out of scope | Why it's later |
| --- | --- |
| Web dashboard | A page where a human signs in to see tasks. v1 has no humans signing in. |
| Subscriptions, invoices, larger packs | One pack size, paid by card, covers launch. |
| SDKs | A packaged npm client. Agents use capabilities/MCP directly. |
| API keys, invitations, receiver pairing | Replaced by Agent Auth and hosted MCP's one-click OAuth. |
| Workflows, fan-out, ordering across lanes | Different product. Lanes give one-at-a-time order within a lane only. |

## Success criteria for v1 launch

- An agent goes from zero to a delivered, signed request in under 5 tool calls, without human help.
- Accepted tasks are never lost: every one reaches `succeeded`, `failed` or `canceled`.
- No task can reach a private network or another project's data (tested).
- p95 delivery lateness under 60 s for tasks due "now", measured on a real deployment.

## Open questions

1. ~~**Remote-only MCP clients.**~~ Resolved: a hosted `/mcp` with one-click OAuth consent (no account), one project per connection.
2. **Key loss.** If an agent loses its keys, its project is gone. Acceptable for a trial?
3. **Abuse.** New keys are new identities (Sybil). Is a global trial cap + per-IP registration limit enough for launch?
4. ~~**What happens after the trial?**~~ Resolved: the agent's person buys attempt packs (*Buying attempts*).
