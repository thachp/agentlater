# 1. Rotating SECRETS_KEY with a previous key and a reseal sweep

Date: 2026-11-08. Status: accepted.

## Context

Every project secret and every hook verify secret is sealed with a key derived from one
`SECRETS_KEY` per environment (`apps/api/src/sealing.rs`). Until now it could not change: a new key
would leave every stored value unreadable, failing each task that needs a secret
(`secret_unreadable`) and answering every verified hook with 503. So a leaked key could not be
replaced, and the risk table in ARCHITECTURE.md carried "`SECRETS_KEY` is never rotated".

Sealed values live in two columns of each project's ledger (`secrets.sealed`,
`hooks.sealed_secret`). Each ledger is its own Durable Object, so there is no single table to
rewrite. Values are opened in two places: the delivery consumer, with ciphertext the ledger hands
out when an attempt is claimed, and the ledger's entry code when it checks a hook's signature.

## Decision

1. **Two keys during a rotation, and no format change.** `SECRETS_KEY` seals and opens.
   An optional `SECRETS_KEY_PREVIOUS` only opens. Opening tries the current key and then the
   previous one. AES-GCM authenticates, so the wrong key fails cleanly rather than returning
   garbage. That means the sealed format (`v1:`) needs no key id, and values sealed before this
   change stay readable without a migration.
2. **Each ledger reseals itself on request.** A new API route seals again, under the current key,
   every value that only the previous key opens. It answers with what it resealed, what no key opens,
   and `rotation`, a one-way fingerprint of the pair of keys (null when there is no previous key).
   The route needs a new gateway assertion scope, `maintain`, which no agent or MCP capability is
   ever granted.
3. **The gateway sweeps.** An hourly cron on the gateway walks `runlater_project` in project-id
   order, 200 projects a run, keeping its place in a one-row D1 table. A full pass that reseals
   nothing marks the rotation complete. When the fingerprint changes (a new rotation, or an undo),
   the sweep starts a fresh pass. When it goes null, the sweep resets. Outside a rotation, each run
   costs one ledger call.
4. **Two key slots in GitHub.** GitHub secrets can't be read back, so nobody can copy the current
   key into a "previous" secret. Instead the key has two slots, `SECRETS_KEY` and `SECRETS_KEY_2`.
   `SECRETS_KEY_SLOT` picks the current slot, and while `SECRETS_KEY_ROTATING` is true the deploy
   passes the other slot as `SECRETS_KEY_PREVIOUS`. `scripts/rotate-secrets-key.mjs` runs `start`,
   `undo` and `finish`, and only ever writes keys. `start` sets the rotating flag before it switches
   the slot, so a deploy between the two still hands the API both keys. `finish` deletes a key, so
   it checks first: the sweep's row must show a pass completed over an hour ago, for the slot the
   deploy reports as current, and checked since the slot variables last changed. Without that,
   `undo` followed quickly by `finish` would read an old completion and delete the only key
   that opens every value. `wrangler deploy --secrets-file` never deletes a
   secret, so the deploy explicitly deletes `SECRETS_KEY_PREVIOUS` once a rotation is finished.
   The steps are in `docs/DEPLOY.md`.

## Alternatives rejected

- **A key id in every sealed value (`v2:<kid>:…`).** It makes opening exact, but every stored
  value would need rewriting to the new format first, and so would the released-format test.
  Trying two keys costs one more key derivation and decryption, and only on values not yet
  resealed. That is fine for one rotation window.
- **Resealing lazily, when a value is next opened.** A project whose secrets are never used
  would never be resealed, so we could never know when the old key is safe to drop.
- **An operator script calling each project.** A script can't reach a Durable Object except
  through the Workers, and that would need a new admin credential. The gateway already holds
  both the project list and the signing key.
- **A boolean "rotating" flag instead of a fingerprint.** The sweep then couldn't tell an undo,
  or a second rotation started soon after the first, from the one it had already finished, and
  would leave values sealed under a key that is about to be dropped.

## Consequences

- A leaked key can now be replaced without failing anyone's deliveries.
- During a rotation, a stolen copy of the ledgers is still exposed to the old key until the sweep
  finishes. The sweep makes that window a few days, where before it had no end.
- Losing the current key still loses every value. Rotation doesn't help against loss, only
  against leaks.
- The `maintain` scope and the cron add surface. The scope is minted only by the gateway's cron,
  never from an agent's identity.
- After a rotation the current key may sit in `SECRETS_KEY_2`. Anything that reads the key has to
  go through the slot variable, as the deploy does.
