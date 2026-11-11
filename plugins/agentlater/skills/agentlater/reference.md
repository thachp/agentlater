# AgentLater reference

The full spec is at https://agentlater.io/docs. This is what an agent needs most often.

## Task lifecycle

```
scheduled ──(run_at reached)──▶ delivering ──2xx──▶ succeeded
    │                               │
    │ cancel                        └─non-2xx/timeout─▶ retrying ──(backoff)──▶ delivering
    ▼                                                      │
 canceled                                     attempts exhausted ──▶ failed ──redrive──▶ scheduled
```

- Retries: 5 attempts by default (`max_attempts` 1–5). Retries wait 10 s, 2 min, 20 min and 40 min, with jitter, so five attempts span 31 to 62 minutes; `Retry-After` is honoured up to 40 min. A task that keeps failing holds its queue and delays its callback or alert for that long, so pass a lower `max_attempts` when you need to hear sooner.
- Retried: 408, 425, 429, 5xx, timeouts (10 s), network errors. Other 4xx and 3xx fail at once. Redirects are not followed.
- Watches and wakes can't be redriven; start a new watch instead.

## Limits

| Limit | Value |
| --- | --- |
| Request body | 64 KB |
| Headers | 32 headers, 8 KB total |
| Furthest `run_at_ms` / `delay` | 7 days, and before the trial ends |
| Trial | 1,000 delivery attempts, 7 days |
| Attempt pack | 50,000 attempts for $5, balance valid 12 months |
| Tasks held | 1,000 (20,000 once a pack is bought) |
| Crons | 10 per project, at most one run an hour each |
| Watches | poll every 5 min–24 h, deadline at most 7 days |
| Captured response | first 16 KiB, deleted 7 days after the task finishes |
| Inbox | `read_inbox` returns at most 50 (default 10); unread kept 30 days |
| Secrets | 50 per project, 16–4096 bytes, 1–20 exact hosts, header values only |
| Hooks | 20 per project; 1,000 events a day per project; 100 a day per hook without `verify` |

## Common errors

| Code | What to do |
| --- | --- |
| `budget_exhausted`, `trial_expired` | `get_payment_link`, give the user `payment_url` |
| `trials_paused` | New trials are full for today; call `create_project` again later |
| `rate_limited` | Over 300 calls in a minute; wait a minute, then batch or slow down |
| `secret_not_found`, `secret_host_not_allowed` | `set_secret` with the destination's exact host in `allowed_hosts` |
| `watch_not_redrivable`, `wake_not_redrivable` | start a new watch / `set_wake` again |
| `ack_beyond_inbox` | ack only up to the last `seq` you read |
| `hook_limit`, `secret_limit` | delete one you no longer need |
| `invalid_request` with `field` | fix that argument and call again |

## Inbox events

| `event` | From |
| --- | --- |
| `task.finished` | a task or watch with `callback_url: "inbox"` (the task, with `response` if captured) |
| `cron.failing`, `cron.recovered` | a cron with `alert_url: "inbox"` |
| `hook.received` | a request to a hook URL (`request.headers`, `request.body`, `verified`) |
| `wake.disabled` | the wake failed 3 times in a row and turned itself off |

## Verifying AgentLater's requests

When the user owns the destination, it can check that a request came from AgentLater. Each request carries `RunLater-Task-Id`, `RunLater-Attempt`, `RunLater-Timestamp` (unix seconds) and `RunLater-Signature: v1=<hex>`: HMAC-SHA256 with the project's signing secret over `"{timestamp}.{task_id}.{attempt}."` followed by the raw body. The secret is the `signing_secret` that `create_project` returns; `get_project` doesn't show it, so call `create_project` again when the user needs it (same project, same secret) and give it only to them. During rotation the header holds several comma-separated `v1=` entries; accept any. Reject timestamps more than 5 minutes off. Ready-made Node and Python verifiers are at https://agentlater.io/docs/delivery.
