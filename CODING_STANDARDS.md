# Coding standards

The goal is less code and fewer bugs. These rules are the ones the code already follows or CI
enforces; domain words are defined in [GLOSSARY.md](GLOSSARY.md).

## Rules

1. **Tests live beside the module they test**, in its own `#[cfg(test)] mod tests`.
   Helpers that several of the ledger's test modules share live once, in
   `apps/api/src/ledger/testing.rs`.
2. **One error constructor per shape.** A validation error becomes a response through
   `ApiError::invalid`; any other error names the request field at fault with
   `ApiError::in_field`. Don't hand-build an `ApiError` literal or add a second mapping.
3. **The ledger validates requests, not the worker entry.** `worker_entry.rs` parses and routes;
   the ledger decides whether a task, cron or watch request is acceptable.
   Debt: secret and hook requests are still validated in `route()`.
4. **A refactor shrinks production code or leaves it level.** Moving code without making it
   shorter or clearly easier to follow doesn't count. Every PR reports its net lines, with
   production and test lines apart (Rust production code is each `.rs` file up to its first
   `#[cfg(test)]`).
5. **Export only what another file reads.** `pub` in Rust and `export` in TypeScript are
   promises; make neither by default.

## Enforcement

CI fails on any of these, so fix the code instead of silencing the check.

- `[workspace.lints]` in `Cargo.toml`, run by `cargo clippy -- -D warnings`:
  `unreachable_pub`, `unused_qualifications`, `unused_lifetimes`, `unused_macro_rules`,
  `clippy::redundant_clone` and `clippy::too_many_lines`.
- `clippy.toml` caps a function at 110 lines. Lower the cap as functions shrink; never raise it.
  The one exemption is `route()`, marked `#[expect]` until it is flattened.
- `npm run knip` (config in `knip.jsonc`): no unused files, dependencies or exports in the
  TypeScript workspaces.

Rules 1 to 4 have no lint; reviewers hold them.
