// @ts-check
// Whether `finish` in ../rotate-secrets-key.mjs may delete the previous SECRETS_KEY
// (docs/adr/0001-rotating-secrets-key.md). A wrong yes deletes the only key that opens whatever is
// still sealed under it, so anything this doesn't recognise is a refusal. It reads nothing itself,
// not even a clock: the script hands it the row, the slot and the time of the last change. Plain
// JavaScript, so `node` runs the script with no build; tested in
// apps/gateway/test/finish-verdict.test.ts, beside the sweep that writes the row.

const HOUR = 3600 * 1000;

/** A time as the sweep's `Date.now()` and the script's `Date.parse` give it: epoch milliseconds. */
const isTime = (/** @type {unknown} */ v) => Number.isSafeInteger(v) && /** @type {number} */ (v) > 0;

/**
 * @param {object} facts
 * @param {unknown} facts.row The gateway's `secrets_rotation` row as `wrangler d1 execute --json`
 *   gives it (apps/gateway/src/rotation.ts writes it), or nothing when the table is empty.
 * @param {1 | 2} facts.current The slot `SECRETS_KEY_SLOT` names now.
 * @param {number} facts.changedAt When a `SECRETS_KEY_` variable last changed, epoch milliseconds.
 * @returns {{ finish: true } | { finish: false, why: string }} `why` is what the script prints.
 */
export function finishVerdict({ row, current, changedAt }) {
  const refuse = (/** @type {string} */ why) => ({ finish: /** @type {const} */ (false), why });
  if (row === undefined || row === null) return refuse("the sweep hasn't completed a pass yet");
  if (typeof row !== "object") return refuse("the sweep's row isn't one this script knows how to read");
  const { rotation, slot, completed_at, checked_at } = /** @type {Record<string, unknown>} */ (row);
  if (rotation === null || completed_at === null) return refuse("the sweep hasn't completed a pass yet");
  // D1 gives the TEXT columns as strings and the INTEGER ones as numbers. A column that is missing
  // or of another type would compare as NaN, and NaN passes every `<` below.
  if (typeof rotation !== "string" || rotation === "" || !isTime(completed_at) || !isTime(checked_at)) {
    return refuse("the sweep's row isn't one this script knows how to read");
  }
  // The slot stands in for the keys: the fingerprint can't be checked here, since the keys can't be
  // read back. That holds while keys change only through the script and deploys only through CI:
  // `start` is the only step that puts a new key in a slot, and it moves SECRETS_KEY_SLOT to it.
  // Setting a slot's secret by hand, or deploying from a laptop, changes keys this can't see.
  if (slot !== String(current)) return refuse(`the sweep last saw slot ${slot}, not ${current}: deploy, then wait for a new pass`);
  if (!isTime(changedAt)) return refuse("can't tell when the SECRETS_KEY_ variables last changed");
  const checked = /** @type {number} */ (checked_at);
  // A full hour after, not merely after: `changedAt` is GitHub's clock in whole seconds and
  // `checked_at` the gateway's, and a deploy that read the variables before the change can land
  // after a check that follows it.
  if (checked - changedAt < HOUR) {
    return refuse("the sweep hasn't checked the keys an hour or more after the last change; deploy, then wait for the sweep");
  }
  // Both times are the gateway's: once a pass has completed, each hourly run probes the keys and
  // moves `checked_at`. The clock of whoever runs the script decides nothing.
  if (checked - /** @type {number} */ (completed_at) < HOUR) {
    return refuse("the sweep hasn't checked the keys an hour or more after the pass completed; wait for attempts in flight");
  }
  return { finish: true };
}
