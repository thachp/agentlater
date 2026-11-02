# RunLater — Target Architecture & Repo Layout

Status: **draft for review** · Companion to [PRODUCT.md](PRODUCT.md)

## Decisions

| Decision | Choice |
| --- | --- |
| Primary customer | AI agents |
| Identity | Agent Auth (Better Auth `@better-auth/agent-auth` host), plus MCP OAuth for the hosted `/mcp` server: a no-account consent page, one project per grant. API keys, invitations and receiver pairing are removed. |
| Backend | Rust (`workers-rs`) on Cloudflare Workers — task ledger, scheduling, delivery |
| Agent-facing layer | TypeScript Worker — Agent Auth host and capabilities, and a hosted MCP server at `/mcp` |
| Frontend | Next.js on Cloudflare Workers via OpenNext |
| Payments | Stripe Checkout, embedded on `runlater.io/pay` with Stripe.js; the gateway creates sessions and takes Stripe's webhook. Prepaid attempt packs, no accounts or subscriptions. |
| Approach | Clean rebuild in a new layout; port the proven ledger/delivery core; delete milestone code. No migration of beta data. |

## System

```mermaid
flowchart LR
    Agent["Agent<br/>(Agent Auth SDK or MCP bridge)"]
    subgraph CF["Cloudflare"]
        Web["apps/web<br/>Next.js · OpenNext<br/>runlater.io, docs"]
        GW["apps/gateway (TypeScript)<br/>Agent Auth host · capabilities · MCP<br/>D1: hosts, agents, projects"]
        API["apps/api (Rust)<br/>internal REST · auth by service binding"]
        DO["TaskLedger Durable Object<br/>SQLite per project:<br/>tasks, attempts, outbox"]
        Q["Queue: ready"]
        C["Delivery consumer (Rust, same Worker)"]
    end
    Dest["Public HTTPS destination"]

    Agent -->|discover / register / execute capability| GW
    GW -->|service binding + signed project assertion| API
    API --> DO
    DO -->|alarm → outbox| Q
    Q --> C
    C -->|claim lease| DO
    C -->|signed request| Dest
```

### Responsibilities

**`apps/gateway` (TypeScript)** — the only public entry for agents.
- Agent Auth discovery, host/agent registration, capability execution (signature + `htm/htu/ath` binding checks).
- Owns identity tables in D1: `host`, `agent`, `project(host_id → project_id, trial, expiry, revoked_at)`.
- Maps each capability to one internal API call. Never accepts a project id from the caller; derives it from the authenticated host.
- MCP, hosted: `/mcp` (Streamable HTTP, stateless) behind `@cloudflare/workers-oauth-provider` (OAuth 2.1, dynamic client registration, PKCE; clients, grants and tokens in the `OAUTH_KV` namespace). `/authorize` is a one-click consent page with no account; approving creates an owner `mcp:<id>` whose grant carries it, and `runlater_project` maps that owner to a project exactly as it maps an Agent Auth host. Tools are the capabilities, run through the same code path (`runCapability`).
- MCP, local: MCP clients can also run the Agent Auth bridge (`@auth/agent-cli mcp --url <gateway>`), a local stdio MCP server that holds the agent's keys and exposes generic Agent Auth tools (`connect_agent`, `list_capabilities`, `execute_capability`, …). Our capability names, descriptions and input schemas are what the agent reads through them.
- Billing: `get_payment_link` creates a Stripe Checkout Session (`ui_mode: embedded_page`) for the caller's project and returns `runlater.io/pay#key=…&secret=…`. `POST /billing/webhook` verifies `Stripe-Signature`, matches the session's metadata to the owner's project, and calls the API's `POST /v1/projects/self/credits` (scope `billing`) with the session id, which the ledger applies once.
- Rate limits registration and capability calls (Workers Rate Limiting binding).

**`apps/api` (Rust)** — the task authority. Not on a public route.
- Reachable only via service binding from the gateway. Every request carries a short-lived assertion `{project_id, scopes, exp}` signed with Ed25519 by the gateway; the API verifies it.
- One `TaskLedger` Durable Object per project (SQLite): tasks, attempts, idempotency keys, outbox, budget counter.
- Alarm wakes for earliest due task → writes outbox → publishes `{project_id, task_id, generation}` to Queue.
- Queue consumer claims a fenced lease, re-validates destination, signs, sends (10 s timeout), records the attempt, then acks.
- Budget is charged per attempt inside the DO, so it cannot be bypassed.
- Assertion format, scopes and the full route contract: [`packages/contracts/openapi.yaml`](../../packages/contracts/openapi.yaml).

**`apps/web` (Next.js)** — landing page and docs at runlater.io (docs under `/docs`), and `/pay`, which mounts Stripe's Embedded Checkout from the key and client secret in the link's fragment. Statically prerendered; no auth in v1.

### Internal API (gateway → api)

```
POST   /v1/projects                    create (idempotent on host id)
GET    /v1/projects/self
POST   /v1/tasks                       schedule
GET    /v1/tasks?status=&cursor=
GET    /v1/tasks/{id}                  includes attempts
POST   /v1/tasks/{id}/cancel
POST   /v1/tasks/{id}/redrive
GET    /health
```

The OpenAPI file in `packages/contracts` is the source of truth; TS types are generated from it for the gateway.

## Repo layout

```
runlater/
├── apps/
│   ├── api/                 Rust Worker: routes, TaskLedger DO, queue consumer (thin)
│   │   ├── src/
│   │   ├── Cargo.toml
│   │   └── wrangler.jsonc
│   ├── gateway/             TS Worker: Agent Auth host, capabilities, MCP
│   │   ├── src/{index,auth,capabilities,api,projects,store}.ts
│   │   ├── migrations/      D1 SQL
│   │   └── wrangler.jsonc
│   └── web/                 Next.js + OpenNext
│       ├── app/
│       ├── app/docs/        quickstart, capabilities (generated), delivery & signatures
│       └── wrangler.jsonc
├── crates/
│   ├── core/                Pure Rust domain: task state machine, retry policy, limits, validation. No worker deps; native unit tests.
│   └── egress/              Pure Rust: destination policy (URL/host/IP checks) + request signing
├── packages/
│   ├── contracts/           openapi.yaml and capability definitions (src/capabilities.ts), shared by gateway and web
│   └── tsconfig/            shared TS config
├── docs/
│   ├── spec/                PRODUCT.md, ARCHITECTURE.md (this)
│   └── adr/                 one file per significant decision
├── Cargo.toml               workspace: apps/api, crates/*
├── package.json             npm workspaces: apps/gateway, apps/web, packages/*
├── turbo.json               (optional) task runner
└── .github/workflows/       ci.yml, deploy.yml
```

### Environments

One `wrangler.jsonc` per app with `env.staging` and `env.production`. No per-milestone configs (`alpha/`, `beta/`, `staging/` go away). Feature flags become runtime vars, not Cargo features — the Rust build has **zero** feature flags.

### Testing

| Layer | Tool |
| --- | --- |
| `crates/*` | `cargo test` (native, fast) |
| `apps/api` | `vitest` + `@cloudflare/vitest-pool-workers` against Miniflare (DO, alarms, queues) |
| `apps/gateway` | `vitest` unit tests; `npm run test:e2e` runs the built gateway and API together in Miniflare, driven by the real `@auth/agent` SDK and the `@auth/agent-cli` MCP bridge |
| End-to-end | One script: register agent → create project → schedule → local receiver gets signed request → `get_task` shows `succeeded` |
| `apps/web` | `next build` + Playwright smoke |

Replaces the 89 files in `scripts/` and the bespoke shell harnesses.

### CI/CD

- `ci.yml`: `cargo fmt/clippy/test`, `npm run lint typecheck test`, `wrangler deploy --dry-run` per app.
- `deploy.yml`: on `main`, deploy to staging, run e2e against staging, then manual approval → production.

## What gets ported vs deleted

| Today | Fate |
| --- | --- |
| `src/core.rs`, `limits.rs`, `security.rs`, `signed.rs`, `callback.rs` (signing), `destinations.rs` / `safe_egress.rs` (policy) | **Port** into `crates/core` and `crates/egress`, cleaned up |
| `src/runtime.rs`, `publication.rs`, `transport.rs`, `delivery.rs`, `application.rs` (CQRS, outbox, leases) | **Port** the patterns into `apps/api`; one runtime instead of four |
| `production_runtime.rs`, `public_beta*.rs`, `invitation*.rs`, `anonymous_trial.rs`, `funding.rs`, `funded_tasks.rs`, `receiver_pairing.rs`, `mcp_delegation.rs`, `general_http*.rs`, `identity.rs`, `auth.rs`, `staging.rs` | **Delete** (keep in git history) |
| `agent-auth/canonical/` | **Port** into `apps/gateway` (TypeScript) |
| `agent-auth/{replacement,coordinator,prototype}` | Delete |
| `mcp/` (OAuth MCP server) | Delete; capability → tool mapping reused in gateway |
| `egress/` | Fold into `crates/egress` |
| `web/next` | Move to `apps/web`; drop `web/landing`, `web/redirect`, `web/docs`, `web/shared` |
| `alpha/`, `beta/`, `staging/`, `offline/`, `discovery/`, `examples/`, `scripts/`, `tests/` | Delete; replaced by per-app tests and one e2e |
| `docs/*.md` (40 files) | Moved to `legacy/docs/`; deleted at cutover ✅ |

## Migration plan

Each phase is one PR that leaves `main` green.

1. **Skeleton.** ✅ Create workspaces, `crates/`, `apps/`, `packages/`. Move old code under `legacy/` so nothing is lost while porting. CI runs the new layout only; legacy workflows (including the old API auto-release) are parked in `legacy/.github/workflows` and no longer run.
2. **Core crates.** ✅ Port domain + egress into `crates/core` and `crates/egress` with native tests. `crates/core` models one task as a pure state machine (generation guards stale dispatches, fence guards stale results) so phase 3 can store tasks as SQLite rows instead of the legacy whole-ledger snapshot. Phase 3 must enable `global_fetch_strictly_public` on the API Worker.
3. **API.** ✅ `apps/api`: TaskLedger DO, alarm, outbox, queue consumer, internal routes, assertion verification. The ledger's SQL runs behind a small `Db` trait, so it is tested natively against rusqlite and in workerd unchanged. `npm run test:e2e` in `apps/api` drives the built Worker in Miniflare (DO SQLite, alarms, Queues) with outbound deliveries captured locally.
   - Admission: a project may hold at most as many waiting tasks as it has attempts left, so storage is bounded by the trial.
   - The gateway's Ed25519 public key is the `GATEWAY_PUBLIC_KEY` secret. Queues `runlater-ready[-staging|-production]` must exist before the first deploy.
4. **Gateway.** ✅ `apps/gateway`: Agent Auth host (Better Auth + `@better-auth/agent-auth`) on D1, seven capabilities, Ed25519 assertions to the API, an `AuthStore` Durable Object for JWT replay and rate-limit state, and a per-IP registration rate limit.
   - The MCP bridge is validated end to end: `@auth/agent-cli@0.5.1` (SDK 0.5.1) works against the 0.6.2 server. No hosted `/mcp` endpoint is needed for it.
   - Capability calls are protected by single-use `jti` (replay returns 401) rather than the legacy request-binding check, which the standard SDK and the bridge do not send.
   - A cross-language test vector pins the assertion format: a token signed by the TypeScript gateway is verified by the Rust API's tests.
5. **Web.** ✅ Landing page and docs (`/docs`, `/docs/capabilities`, `/docs/delivery`) in `apps/web`, statically prerendered.
   - The capabilities reference renders the same definitions the gateway serves (`@runlater/contracts/capabilities`).
   - The signature-verification samples (Node, Python) are executed in tests against the `crates/egress` vector, and the limits the site states are checked against `crates/core`.
   - `docs.runlater.io` content now lives at `runlater.io/docs`; the old hostname is retired at cutover, with no redirect.
6. **E2E + deploy.** ✅ One journey script ([`apps/gateway/e2e/journey.mjs`](../../apps/gateway/e2e/journey.mjs)) runs locally in CI and against staging after every staging deploy, delivering to a staging-only receiver Worker (`apps/receiver`). Staging failing blocks production. First live run still needs the setup in [DEPLOY.md](../DEPLOY.md).
7. **Cutover.** ✅ `runlater.io` and `gateway.runlater.io` serve the new Workers; the legacy hostnames, Workers and queues are deleted, and so is `legacy/`. See [DEPLOY.md](../DEPLOY.md).

## Risks

- **MCP compatibility** — Agent Auth isn't standard MCP OAuth, so remote MCP clients use the hosted `/mcp` server and its OAuth consent page instead; the `@auth/agent-cli` bridge still works for clients that run local servers. Both are covered by `npm run test:e2e`.
- **Agent Auth maturity** — packages are at 0.6.x and the protocol may change. Keep it isolated in `apps/gateway/src/auth`.
- **Removing the live MCP/OAuth path** breaks any current users at cutover. Phase 7 must check for live tasks/grants first.
