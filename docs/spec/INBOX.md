# AgentLater — Inbox and response capture

Status: **implemented** · Owner: Patrick Thach · Last updated: 2026-11-02 · The rules are in [PRODUCT.md](PRODUCT.md#inbox-and-captured-responses); this note is the why.

## One line

**An agent can learn what happened while it was gone, without running a server.**

## Problem

AgentLater let an agent *act* later. It did not let the agent *find out* later.

- Every way AgentLater reported back (`callback_url`, a watch's callback, a cron's `alert_url`) was a signed `POST` to a public HTTPS URL. Most agents have none: Claude Code, Claude Desktop, Cursor and every other client of the hosted `/mcp` live in a session, not on a host.
- A task's attempts recorded `http_status` and `outcome`, never what the destination said. "Call this API at 9am" ended with the agent knowing it got a `200`, not what came back. Only watches kept a response.

The constraint on an agent's throughput is the session boundary: it exists only while someone talks to it. AgentLater already let work *continue* across that boundary. The inbox lets *results* cross it, so the next session starts from a report rather than re-investigating.

```
Session 1 ──schedule_http { url, delay: "12h", capture: true, callback_url: "inbox" }──▶ AgentLater
          … session ends …
AgentLater ──signed request──▶ destination ──200 {"status":"failed","step":"migrate"}
AgentLater ──task.finished (with response)──▶ inbox
Session 2 ──get_project──▶ inbox_unread: 1
Session 2 ──read_inbox { ack: true }──▶ [{ seq: 1, event: "task.finished", task: {…, response} }]
```

## Decisions

| Question | Decision | Why |
| --- | --- | --- |
| How to target the inbox | The literal `"inbox"` in `callback_url` / `alert_url` | Small surface; reads as "call back to the inbox". `inbox` can never be a valid URL, so there is no ambiguity. |
| Inbox event or callback task | An event written in the ledger transaction that finishes the task | It cannot fail, needs no attempt or task slot, and survives the budget running out. |
| Event shape | The callback/alert payload (`event`, `task` / `reason`, `task_id`, `cron`) plus `seq` and `at_ms` | An agent handles a callback and an inbox event with the same code. |
| Where the cursor lives | On AgentLater's side, one per project | The agent can't keep one; that's the whole problem. |
| Read vs ack | Reading doesn't advance; `ack_inbox` does, or `read_inbox(ack: true)` | At-least-once: a session that dies mid-handling leaves events for the next one. `ack: true` keeps the common case to one call. |
| Ack past the newest event | `400 ack_beyond_inbox` | It means the agent is acking events it has not read: a bug worth surfacing. |
| Ack scope | `read` | Acking is part of reading. There is one cursor per project and one owner per project, so a read grant that acks only hides events from that same owner. |
| `read_inbox(after, ack: true)` with `after` past the cursor | `400` on `after` | Acking to the last event returned would ack the unread ones skipped over. |
| Page size | 1–50, default 10 | Every event lands in the agent's context; one with a captured response is ~20 KiB. |
| Full inbox | Drop the oldest acked event, else the oldest unread, and count it in `dropped` | Never silently lose unread work, never refuse a finish. |
| Captured body | First 16 KiB, as UTF-8 with invalid bytes replaced; no base64 | Matches `watch.last_body`. Agents read text; binary responses are rare in this use. |
| Captured headers | `content-type`, `location`, `etag`, `retry-after` | Useful for deciding what to do next; never `set-cookie` or auth. |
| Capture retention | 7 days after the task finishes, shorter than the task's 30; inbox events lose their copy of the body at the same time | Responses can hold credentials (a token exchange returns a token). |
| A slow or broken captured body | Record status and headers with `body: null`, `truncated: true`; never change the outcome | The destination already answered. Turning that into a retry would deliver twice, and keeping the earlier capture would show an old status for a newer answer. |
| Marking responses untrusted | In tool descriptions and docs, not a field | A field that is always `true` is noise. |

Rejected:

| Model | Why not |
| --- | --- |
| Read = delete (pop) | A session that crashes after reading loses the event. |
| SQS-style visibility timeout | Lease durations are hard for agents, which have no sense of elapsed time. |
| Cursor kept by the client | Agents can't keep it across sessions. |
| Push over MCP notifications | The hosted `/mcp` server is stateless and sessions are short. |

## Found while building

Durable Object SQLite refuses compound `SELECT`s with more than a few terms, which a wider `next_wake` union hit only in workerd. The native test database now enforces the same limit (`SQLITE_LIMIT_COMPOUND_SELECT = 5`), so `cargo test` catches it.

## Later

| Next | Notes |
| --- | --- |
| ~~**Wake.**~~ | Built: `set_wake`, see [PRODUCT.md](PRODUCT.md#wake). |
| ~~Callbacks and capture for cron runs~~ | Built: `create_cron`'s `callback_url`, `capture` and `report`. |
| Notes an agent writes to its own future inbox | AgentLater as timed memory, not only an HTTP scheduler. A product decision of its own. |
| Several inboxes or cursors per project | One owner per project in v1, so one reader. |
