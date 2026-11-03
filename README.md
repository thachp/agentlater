# RunLater

Let an AI agent make an HTTP request later, and check whether it happened.

The product spec is [docs/spec/PRODUCT.md](docs/spec/PRODUCT.md) and the design is in
[docs/spec/ARCHITECTURE.md](docs/spec/ARCHITECTURE.md). The pre-rebuild implementation was removed at
cutover; it is in git history before this change.

## Layout

| Path | What | Stack |
| --- | --- | --- |
| `apps/api` | Private task API: ledger, scheduling, delivery | Rust · workers-rs |
| `apps/gateway` | Public entry for agents: Agent Auth, capabilities, MCP | TypeScript Worker |
| `apps/web` | runlater.io landing and docs | Next.js · OpenNext |
| `crates/core` | Task domain, no Cloudflare deps | Rust |
| `crates/egress` | Destination policy and request signing | Rust |
| `packages/contracts` | OpenAPI contract between gateway and API | OpenAPI 3.1 |
| `packages/tsconfig` | Shared TypeScript config | |
| `plugins/runlater` | Claude Code plugin: the hosted MCP server and a `runlater` skill | Markdown · JSON |

Everything deploys to Cloudflare Workers, with `staging` and `production` environments in each app's `wrangler.jsonc`; see [docs/DEPLOY.md](docs/DEPLOY.md).

## Develop

Requires Rust (pinned in `rust-toolchain.toml`), Node 22+, and `cargo install worker-build --version 0.8.7 --locked`.

```bash
npm install
```

```bash
cargo test && npm run typecheck && npm test
```

```bash
npm run dev -w @runlater/gateway
```

```bash
npm run dev -w @runlater/web
```

## License

The code is [MIT](LICENSE). Using the hosted service at runlater.io is covered by its [Terms of Service](https://runlater.io/terms) and [Privacy Policy](https://runlater.io/privacy).
