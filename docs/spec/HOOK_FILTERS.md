# AgentLater — Hook filters

Status: **implemented** · Owner: Patrick Thach · Last updated: 2026-11-03 · The rules are in [PRODUCT.md](PRODUCT.md#inbound-hooks); this note is the why. Builds on [inbound hooks](HOOKS.md).

## In one line

**A hook keeps only the requests the agent cares about.**

## Problem

A sender pushes every event you subscribe it to, and GitHub's choices are coarse. Subscribe a repo to "Workflow runs" and every run sends three requests: `requested`, `in_progress` and `completed`. Only a completed run that failed means anything to an agent fixing CI. Today each of those requests:

- becomes an inbox event, which the agent reads into its context;
- counts against the project's 1,000 events a day, or an unverified hook's 100;
- can wake the agent, starting a session that finds nothing to do.

So the agent pays for noise in context, in wake-ups and in daily cap. Filtering at the sender only goes as far as event type, never "only failures".

## What we're building

### `accept` on `create_hook`

`accept` is a list of up to 5 rules. A request is kept if **any** rule matches. If `accept` is left out, everything is kept, as today.

```json
{
  "name": "ci-failures",
  "verify": { "scheme": "github", "secret": "…" },
  "accept": [
    {
      "headers": { "x-github-event": ["workflow_run"] },
      "path": "/workflow_run/conclusion",
      "in": ["failure", "timed_out"]
    },
    { "headers": { "x-github-event": ["pull_request_review"] } }
  ]
}
```

Within one rule, **every** part given must hold:

| Part | Matches when | Limits |
| --- | --- | --- |
| `headers` | Each named header (case-insensitive name) has one of the listed values, compared exactly. | 1–5 names per rule, 1–20 values each, 256 bytes per value |
| `path` + `in` | The body parses as JSON and the value at this JSON Pointer is one of `in`. | The same rules as a watch's `until`: scalars only, numbers compared by value, 1–20 values |

- **Every rule needs at least one part.** An empty rule `{}` would match everything, which is what leaving out `accept` already does.
- **A body that isn't JSON, or a missing path, doesn't match.** No rule can be partly checked.
- **Matching reads the whole request**, up to 256 KiB, not just the 16 KiB kept on the event.

### What happens to a request that's filtered out

- **It gets the same `202` as a kept request.** If the sender saw an error it would retry. (Once a cap is reached, a matching request gets `429` while a filtered one still gets `202`, so someone holding an unverified hook's URL could learn its filter by probing. On an unverified hook a filter is no protection anyway.)
- **Nothing is written:** no inbox event, no wake, and it doesn't count toward any daily cap.
- **It doesn't use the hook's rate limit (60 a minute) either.** A busy repo sends bursts of events, and noise must not get the one request that matters refused. A request is only matched once it is within the limit, so a flood turned away costs no matching; the gateway's limits per IP and per project bound the rest.
- **`list_hooks` counts it** under `filtered`, so an agent can see that the hook works but nothing has matched yet.
- **Order:** signature, then the rate-limit check, then filter, then dedupe, then the daily caps. A forged request is still refused with `401` before any filter runs. A filtered request records no delivery id, so a later retry of it is checked again from the start.

### Changing a hook without changing its URL: `update_hook`

`create_hook` with an existing name issues a **new URL**. That's right when rotating a leaked URL, but tightening a filter would then mean registering the URL with GitHub all over again. So:

| Capability | Input | Scope | Returns |
| --- | --- | --- | --- |
| `update_hook` | `name`, `verify?`, `accept?`, `keep_headers?` | `schedule` | the hook, with no URL; the URL stays the same |

- Fields left out stay as they are. `accept: null` removes the filter, and `verify: null` makes the hook unverified.
- Changing `verify` replaces the secret, for when you rotate it at the sender.
- The counts are kept, and so are the delivery ids seen in the last 24 hours.
- `404 hook_not_found` if there's no hook with that name.

### Views

`list_hooks` (and `create_hook` and `update_hook`) show `accept` exactly as stored, plus a `filtered` count alongside `received` and `refused`.

## Decisions

| Question | Decision | Why |
| --- | --- | --- |
| Match language | `headers` plus watch-style `path` + `in` | Agents already know `until`, and the matcher exists and is tested. Regexes and expressions are slow and easy to get wrong, and they invite a templating language. |
| How rules combine | AND within a rule, OR across up to 5 rules | That covers "failed workflow runs, or review requests" without nesting. |
| Answer to the sender when filtered | `202` | A non-2xx makes senders retry and then disable the webhook. |
| Filter before or after the signature | After | A forged request should get `401` whatever its content. That keeps the `refused` count honest, and the filter can't be used to find out whether a signature was valid. |
| Daily cap and rate limit | Filtered requests use neither | The cap protects the inbox, which a filtered request never reaches; the rate limit must not let noise crowd out a match. The gateway's per-IP and per-project limits bound the work. |
| Unverified hooks | Filters allowed, but they add no security | Anyone with the URL can send whatever matches, so the docs say this and still recommend `verify`. |
| Changing a filter | `update_hook`, keeping the URL | Re-registering a webhook at every sender because a filter changed would push agents to skip filters. |

## Out of scope

- Hooks that send a request of their own (like a watch's `on_match`). It would let an outsider spend the project's attempts. This needs `verify` to be required and its own spec.
- Changing the event, e.g. keeping only some fields of the body.
- Matching that isn't exact (regexes, ranges, `not`).

## Settled

1. **`update_hook` is its own capability.** "Same name gives a new URL" stays the way to rotate a leaked one.
2. **5 rules of 5 headers** each.
3. **Filtered requests use no daily cap.** Review found they shouldn't use the per-hook rate limit either (noise could crowd out a match), so they don't; matching is cheap (headers first, the body parsed at most once) and runs only within the limit.

## Work involved

- Core: `accept` validation and matching in `hook.rs`. The `path`/`in` check calls `Until`'s matcher instead of duplicating it.
- Ledger: migration 12 adds `accept` and `filtered` to `hooks`; `update_hook`; the filter step in `receive_hook`.
- API: `POST /v1/hooks/{name}` for the update. The verify secret is re-sealed when it changes.
- Gateway and contract: `update_hook`, `accept` in the `create_hook` schema, D1 migration 0010 to grant the new capability.
- Docs: PRODUCT.md § Inbound hooks, the delivery page, HOOKS.md (a pointer to this spec).
- Tests:
  - core: rule matching, including non-JSON bodies, header case and the 5×5 limits
  - ledger: filtered requests aren't written, don't wake, don't count toward the cap, and are counted; `update_hook` keeps the URL and counts
  - workerd e2e: a GitHub `workflow_run` success is filtered out, a failure is kept
  - a journey step on staging
