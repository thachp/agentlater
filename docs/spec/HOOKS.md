# RunLater — Inbound hooks

Status: **implemented** · Owner: Patrick Thach · Last updated: 2026-11-03 · The rules are in [PRODUCT.md](PRODUCT.md#inbound-hooks); this note is the why. Builds on the [inbox](INBOX.md), wake and secrets.

## In one line

**An agent with no server can receive webhooks.**

## Problem

Agents wait on the outside world in two ways:

- **Polling:** check a status URL until it says done. `watch_http` covers this, and `on_match` now acts on the result.
- **Being pushed to:** GitHub, Stripe, CI providers, Linear and most SaaS tools send a webhook when something happens. RunLater has nothing for this.

A webhook needs a public HTTPS URL that stays up, and most agents (Claude Code, Desktop, Cursor, anything behind the hosted `/mcp`) don't have one. So today they poll things that would happily push, or they don't find out at all. Polling costs one delivery attempt per check. It is slow to notice, and some events have no status URL to poll: "a PR review was requested", "a payment was disputed".

The inbox and wake already carry results across the session boundary. Inbound hooks let outside events cross it the same way:

```
Agent    ──create_hook { name: "gh-ci", verify: { scheme: "github", secret: "…" } }──▶ RunLater
RunLater ──{ url: "https://hooks.runlater.io/h/prj_…/hk_…" } (shown once)──▶ Agent
Agent    registers the URL as a GitHub webhook … session ends …
GitHub   ──POST workflow_run (X-Hub-Signature-256)──▶ hooks.runlater.io
RunLater checks the signature, writes hook.received to the inbox, answers 202, and wakes the agent if a wake is set
Session 2 ──read_inbox──▶ [{ event: "hook.received", hook: "gh-ci", verified: true, request: {…} }]
```

## What we're building

### Capabilities

| Capability | Input | Scope | Returns |
| --- | --- | --- | --- |
| `create_hook` | `name`, `verify?`, `keep_headers?` | `schedule` | the hook, including `url`, **shown this once** |
| `list_hooks` | — | `read` | the hooks, without URLs or secrets |
| `delete_hook` | `name` | `cancel` | the deleted hook; its URL starts answering `404` |

- **`name`** follows the cron name rules: `A-Za-z0-9_.:-`, 1–64 characters, starting with a letter or digit.
- **Calling `create_hook` again with the same name** replaces the hook with a new URL, and the old URL stops working at once. That is how you rotate a URL that leaked.
- **Limits:** at most 20 hooks per project (`429 hook_limit`).

### The URL

`https://hooks.runlater.io/h/{project_id}/{token}`

- **The token** is `hk_` followed by 32 random bytes, hex-encoded. RunLater stores only its SHA-256, so even a copy of the database can't recreate the URL. (The URL does appear in the gateway's request logs, Workers Logs, which only operators can read and which expire.) Lookup is by hash, so there is no timing-sensitive string comparison.
- **The project id** is in the path so the gateway can route to the right ledger without a global index. It isn't a secret: it already appears in every signed delivery.
- **A separate host** (`hooks.`), served by the gateway Worker on its own route. Agent traffic and the open internet stay on different names, so each can be rate-limited, logged and, if need be, shut off on its own.

### Verification

`verify` is optional, but the docs recommend it whenever the sender signs its requests.

| `scheme` | Checks | Header |
| --- | --- | --- |
| `github` | HMAC-SHA256 of the raw body | `X-Hub-Signature-256: sha256=<hex>` |
| `stripe` | HMAC-SHA256 of `"{t}.{body}"`; `t` must be within 5 minutes | `Stripe-Signature: t=…,v1=…` |
| `hmac_sha256` | HMAC-SHA256 of the raw body, hex or base64 | `header` you name, with an optional `prefix` such as `sha256=` |

- **`verify.secret` is the value itself, given at creation.** It is sealed with the secrets key, using AES-GCM with its own name in the associated data, and is never shown again.
  - It does **not** come from `set_secret`. A stored secret means "send this only to these hosts", and verification sends nothing, so reusing one would blur that rule.
- **A verified hook refuses a bad or missing signature** with `401`, and writes nothing to the inbox (only the hook's `refused` count). Senders show this in their delivery logs, which is where a person debugging the setup looks.
- **An unverified hook accepts any POST** to its URL. Its events are marked `verified: false`, so anyone who learns the URL can put text in the agent's inbox. The capability description and `read_inbox` say so plainly.

### What a delivery becomes

The ledger writes one inbox event, in one transaction, before RunLater answers the sender:

```json
{
  "seq": 42, "at_ms": 1767225600000, "event": "hook.received",
  "hook": "gh-ci", "verified": true, "delivery_id": "72d3162e-cc78-11e3-81ab-4c9367dc0958",
  "request": {
    "method": "POST",
    "headers": { "content-type": "application/json", "x-github-event": "workflow_run" },
    "body": "{\"action\":\"completed\",…}", "body_bytes": 18422, "truncated": true
  }
}
```

- **Body:** up to 16 KiB is kept, with `body_bytes` and `truncated` showing what was cut. A request over 256 KiB is refused with `413` before it reaches the ledger. Senders rarely go above that, and it bounds what the gateway reads. The signature is checked against the full body, not the kept part.
- **Headers:** only an allowlist is kept.
  - Always kept: `content-type` and `user-agent`.
  - Each scheme adds its event and delivery headers: `x-github-event`, `x-github-delivery` and `x-github-hook-id` for `github`.
  - `keep_headers` adds up to 10 more names.
  - Never kept, even if named: `authorization`, `cookie`, `proxy-authorization` and the signature header itself.
- **`delivery_id`** comes from the scheme's delivery header (`X-GitHub-Delivery`, or the Stripe event `id` in the body), and is null otherwise.
  - A `delivery_id` this hook has already seen in the last 24 hours gets `200` and is not written again, because senders retry.
  - Without a delivery id, a retry shows up as a second event. The docs tell agents to dedupe on their own key.
- **Wake:** a hook event wakes the agent like any other inbox event, with the same debounce.
- **Treat as data:** the body is written by whoever sent it. `read_inbox` already warns about captured responses, and the warning now covers hook events too.

### Answering the sender

| Status | When |
| --- | --- |
| `202` | Written to the inbox. |
| `200` | A duplicate `delivery_id`; nothing new written. |
| `401` | A verified hook got a bad, missing or stale signature. |
| `404` | Unknown token, deleted hook, or unknown project. These are all the same, so the response reveals nothing. |
| `405` | Not `POST`. A `HEAD` or `GET` to a live URL also gets `405`, so a URL can't be probed for liveness without sending a body. |
| `410` | The project's trial or balance has expired, which tells senders to stop. |
| `413` | Body over 256 KiB. |
| `429` | Rate limited, with `Retry-After`. |

The response body is always empty JSON (`{}`), and RunLater never sends back anything the sender wrote.

### Limits and cost

- **Rate:** per hook, at most 60 requests a minute (GCRA, burst 20); per project, 1,000 hook events a day, 10,000 once a pack is bought; per unverified hook, 100 a day.
  - Both are checked in the ledger, so they hold across gateway instances.
  - Refusals are counted on the hook (`refused: { rate_limited: n, … }`), so `list_hooks` can show a noisy sender.
- **Cost:** receiving a hook **costs no delivery attempts**. Attempts pay for outbound requests; an inbox write is what `callback_url: "inbox"` already gives for free. The daily cap and the inbox's existing cap of 10,000 events, which drops the oldest acked event first, keep storage bounded.
- **Stats:** `list_hooks` shows, per hook: `created_ms`, `verify.scheme`, `keep_headers`, `last_received_ms`, `received` and `refused` (counted by reason). It never shows the URL or the secret.

### Routing and trust

- **The public path:** `hooks.runlater.io` → gateway → API, over the existing service binding.
  - The gateway doesn't check Agent Auth on this route. It reads at most 256 KiB, then calls the API's new internal route `POST /v1/hooks/_receive` with the project id, the token, the method, the headers and the raw body.
  - It signs that call with a short-lived project assertion carrying the internal scope `hooks.receive`. No agent can ever be granted that scope.
- **The ledger:** hashes the token, finds the hook, checks the signature (after opening the sealed secret, as the consumer does for secrets), applies the rate limits and dedupe, writes the event and answers. All of this happens in the project's Durable Object, so there is one transaction and no shared state.
- **Agent routes:** `create_hook`, `list_hooks` and `delete_hook` go through the normal gateway routes, with a project assertion and the usual scopes.

## Decisions

| Question | Decision | Why |
| --- | --- | --- |
| Push to the agent, or to the inbox? | The inbox, plus wake | The agent has no URL, which is the whole point. The inbox already handles retention, acks and dropping. |
| Show the URL again later? | No, only at creation; rotate by creating again | The URL is a credential, since anyone with it can write to the inbox. A hash can't leak it. Agents register it once, right after creating it. |
| Where the verify secret comes from | Given at `create_hook`, sealed, never shown | Secrets in the store are bound to the hosts they may be sent to; a verify secret is never sent. |
| Unverified hooks? | Allowed, with events marked `verified: false` | Many senders don't sign (forms, simple SaaS, home-grown scripts). Refusing them pushes agents back to polling. |
| Cost | No attempts | An event is an inbox write, like an inbox callback. Abuse is bounded by rate limits and caps, not by charging. |
| Dedupe | By the sender's delivery id, for 24 hours | Senders retry on timeouts. Without a delivery id there is nothing safe to key on. |
| Synchronous challenges (Slack `url_verification`, Meta `hub.challenge`) | Not in v1 | They need RunLater to echo sender input back. That is small, but each scheme needs its own handling. |
| Filtering (e.g. only `workflow_run`) | Not in v1; `keep_headers` keeps the event type visible | Filtering at the sender (GitHub's event picker) covers most cases. |
| Scopes | `schedule` to create, `cancel` to delete, `read` to list | The same as crons and wake. Hook events land in the inbox, which `read` already covers. |

## Out of scope

- Turning a hook into an outbound action (a hook → `on_match`-style request). Possible later, but it would let anyone with the URL spend attempts.
- Answering the sender with anything other than a status code.
- Transforms or templating on what is kept. (Filters came next: [HOOK_FILTERS.md](HOOK_FILTERS.md).)
- Hooks that outlive their project.

## Settled

1. **Its own host.** `hooks.runlater.io` (`hooks-staging.runlater.io`), a second custom domain on the gateway Worker, which serves only `/h/…` there.
2. **1,000 events a day** per project, raised to 10,000 once a pack is bought: a trial shouldn't cap a paying user, and the cap is per UTC day, so it resets predictably.
3. **The URL is shown once.** Only the token's hash is stored.

## Found while building

- **A made-up project id must not create a ledger.** Durable Objects are created on first use, so the gateway checks the project exists (the `runlater_project` table, unique on `project_id`) before forwarding, and answers 404 otherwise. Workers rate limits per client IP (120 a minute) and per project (600 a minute) sit in front, so a flood is turned away before D1 or the ledger. Keyed per URL they would do nothing: anyone who has seen one URL knows the project id and can invent tokens.
- **Missing key.** A verified hook needs `SECRETS_KEY` to open its secret. Without it, creating one is `503 secrets_unavailable`, and a request to one is `503` rather than accepted unchecked, so the sender retries.
- **Credential headers in `keep_headers` are a 400, not silently dropped**, so the agent learns why they are missing.
- **Delivery ids only count when verified.** An unsigned request could claim any id and get a real one dropped as a "retry".
- **One leaked URL must not cost the project its hooks or its inbox.** A hook without `verify` takes anyone's requests, so it has its own cap of 100 events a day, and the project's other hooks keep theirs. A full inbox drops its oldest hook event before any other unread event, and refuses a hook request (`429 inbox_full`) rather than push out an unread task report.
- **Bodies that aren't UTF-8** are kept lossily (each bad byte becomes U+FFFD), so `truncated` is computed on the text, not from `body_bytes`.
- **GitHub signs only the body.** Its delivery id and event headers aren't covered, and there is no timestamp, so someone holding one genuine request (a repo admin reading delivery logs) can replay it under new delivery ids. Stripe's scheme has a timestamp and is not affected. Agents should treat events as untrusted and dedupe on content where it matters.
- **The rate limit is GCRA in one integer column** (`rate_tat_ms`): it needs no timer and no floating point, and a refused request doesn't use up the burst.

## Work involved

- Core: hook name and verify validation, the three signature schemes, the header allowlist.
- Ledger: migration 11 adds a `hooks` table and a `hook_deliveries` table (delivery ids, with expiry). It also adds the receive path and the rate-limit buckets, and handles an inbox event with no task.
- API: the internal routes, the scope, and sealing the verify secret.
- Gateway: the public `hooks.` route, the 256 KiB read cap, and the internal assertion.
- Contract and docs: three capabilities, OpenAPI, `read_inbox`'s event list, PRODUCT.md § Hooks.
- Tests:
  - ledger: the schemes, including bad, stale and missing signatures; dedupe; rate limits; replacing and deleting hooks; the expired project
  - workerd e2e: a signed GitHub-style delivery reaches `read_inbox` and wakes
  - a journey step on staging, with the receiver fixture POSTing to the hook
