// The verdict scripts/rotate-secrets-key.mjs asks before `finish` deletes the previous SECRETS_KEY.
// It is tested here because this suite also has the sweep that writes the row the verdict reads.

import { spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { DatabaseSync, type SQLInputValue } from "node:sqlite";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { finishVerdict } from "../../../scripts/lib/finish-verdict.mjs";
import { type Reseal, d1SweepStore, sweep } from "../src/rotation";

const HERE = dirname(fileURLToPath(import.meta.url));
const HOUR = 3600 * 1000;
const NOW = 1_800_000_000_000;
const CHANGED = NOW - 5 * HOUR;

/** The row as `wrangler d1 execute --json` gives it once a pass has completed: slot 2 is current. */
const complete = {
  id: 1,
  rotation: "0123456789abcdef0123456789abcdef",
  slot: "2",
  cursor: "",
  resealed: 0,
  unreadable: 0,
  completed_at: NOW - 2 * HOUR,
  checked_at: NOW - HOUR,
  resealed_total: 3,
};
/** The row after the sweep reset: no rotation is on. */
const fresh = { ...complete, rotation: null, slot: null, completed_at: null, checked_at: null, resealed_total: 0 };

const ask = (row: unknown, facts: Record<string, unknown> = {}) =>
  finishVerdict({ row, current: 2, changedAt: CHANGED, ...facts } as Parameters<typeof finishVerdict>[0]);
const without = (column: string) => Object.fromEntries(Object.entries(complete).filter(([name]) => name !== column));

const FINISH = { finish: true };
const NO_PASS = { finish: false, why: "the sweep hasn't completed a pass yet" };
const UNKNOWN_ROW = { finish: false, why: "the sweep's row isn't one this script knows how to read" };
const NOT_CHECKED = { finish: false, why: "the sweep hasn't checked the keys an hour or more after the last change; deploy, then wait for the sweep" };
const UNKNOWN_CHANGE = { finish: false, why: "can't tell when the SECRETS_KEY_ variables last changed" };
const TOO_SOON = { finish: false, why: "the sweep hasn't checked the keys an hour or more after the pass completed; wait for attempts in flight" };
const otherSlot = (saw: unknown, current: number) => ({
  finish: false,
  why: `the sweep last saw slot ${saw}, not ${current}: deploy, then wait for a new pass`,
});

describe("whether finish may delete the previous SECRETS_KEY", () => {
  it("finishes when a pass completed for the current slot and the keys were checked an hour after it and after the last change", () => {
    expect(ask(complete)).toEqual(FINISH);
    expect(ask({ ...complete, slot: "1" }, { current: 1 })).toEqual(FINISH);
  });

  it("does not judge unreadable: keeping the previous key opens none of those values", () => {
    expect(ask({ ...complete, unreadable: 7 })).toEqual(FINISH);
  });

  it("refuses when there is no row", () => {
    expect(ask(undefined)).toEqual(NO_PASS);
    expect(ask(null)).toEqual(NO_PASS);
  });

  it("refuses a pass that never completed", () => {
    expect(ask(fresh)).toEqual(NO_PASS);
    // Mid-pass: the rotation is known and the keys were checked, but nothing has completed.
    expect(ask({ ...complete, cursor: "p7", resealed: 2, completed_at: null })).toEqual(NO_PASS);
    expect(ask({ ...complete, rotation: null })).toEqual(NO_PASS);
  });

  it("refuses when the sweep saw another slot: an undo, or a row left from an earlier rotation", () => {
    // `undo` moved SECRETS_KEY_SLOT back to 1; the completed pass was for slot 2.
    expect(ask(complete, { current: 1 })).toEqual(otherSlot("2", 1));
    // Even when the sweep has probed since, as it does hourly until a deploy changes the keys.
    expect(ask({ ...complete, checked_at: NOW }, { current: 1, changedAt: NOW - HOUR })).toEqual(otherSlot("2", 1));
    // An API deployed before it reported its slot.
    expect(ask({ ...complete, slot: null })).toEqual(otherSlot(null, 2));
    expect(ask(without("slot"))).toEqual(otherSlot(undefined, 2));
    // D1 gives the TEXT column as a string; a number is not the slot the sweep stored.
    expect(ask({ ...complete, slot: 2 })).toEqual(otherSlot(2, 2));
    expect(ask({ ...complete, slot: "2 " })).toEqual(otherSlot("2 ", 2));
  });

  it("refuses until the keys were checked a full hour after the last change", () => {
    expect(ask(complete, { changedAt: complete.checked_at - HOUR })).toEqual(FINISH);
    expect(ask(complete, { changedAt: complete.checked_at - HOUR + 1 })).toEqual(NOT_CHECKED);
    expect(ask(complete, { changedAt: complete.checked_at })).toEqual(NOT_CHECKED);
    // `undo` twice with no deploy between: the slot matches the old pass again, the check is stale.
    expect(ask(complete, { changedAt: NOW - 60_000 })).toEqual(NOT_CHECKED);
  });

  it("refuses a check within a second of the change, whichever clock says it came first", () => {
    // GitHub gives the change in whole seconds, and its clock is not the gateway's.
    expect(ask(complete, { changedAt: complete.checked_at + 1000 })).toEqual(NOT_CHECKED);
    expect(ask(complete, { changedAt: complete.checked_at - 1000 })).toEqual(NOT_CHECKED);
  });

  it.each([Number.NaN, 0, -1, Number.POSITIVE_INFINITY, undefined, null, "2026-11-12T00:00:00Z"])(
    "refuses when the time of the last change is %s",
    (changedAt) => {
      expect(ask(complete, { changedAt })).toEqual(UNKNOWN_CHANGE);
    },
  );

  it("refuses until the sweep has checked the keys a full hour after the pass completed", () => {
    const completedAt = (completed_at: number) => ask({ ...complete, completed_at, checked_at: NOW });
    expect(completedAt(NOW - HOUR)).toEqual(FINISH);
    expect(completedAt(NOW - HOUR + 1)).toEqual(TOO_SOON);
    // The run that completed the pass, before any later probe.
    expect(completedAt(NOW)).toEqual(TOO_SOON);
    // The next cron run started 700 ms sooner into its minute: under an hour, so the run after it decides.
    expect(completedAt(NOW - HOUR + 700)).toEqual(TOO_SOON);
    expect(completedAt(NOW - 2 * HOUR + 700)).toEqual(FINISH);
    // A check before the completion is not something the sweep writes.
    expect(completedAt(NOW + 5 * HOUR)).toEqual(TOO_SOON);
  });

  it("gives the same answer whatever the laptop's clock says: no time of its own is an input", () => {
    for (const now of [0, NOW - 365 * 24 * HOUR, NOW + 365 * 24 * HOUR, Number.NaN]) {
      expect(ask(complete, { now })).toEqual(FINISH);
      expect(ask({ ...complete, checked_at: complete.completed_at + HOUR - 1 }, { now })).toEqual(TOO_SOON);
    }
  });

  it("refuses times in seconds: every time compared is epoch milliseconds", () => {
    const seconds = { ...complete, completed_at: complete.completed_at / 1000, checked_at: complete.checked_at / 1000 };
    expect(ask(seconds)).toEqual(NOT_CHECKED);
  });

  it.each(["rotation", "completed_at", "checked_at"])("refuses a row with no %s column", (column) => {
    expect(ask(without(column))).toEqual(UNKNOWN_ROW);
  });

  it.each([
    ["a string", String(NOW - 2 * HOUR)],
    ["a fraction", NOW - 2 * HOUR + 0.5],
    ["NaN", Number.NaN],
    ["zero", 0],
    ["negative", -1],
    ["past what a number holds exactly", 2 ** 60],
    ["a boolean", true],
    ["an object", {}],
    ["a list", [NOW - 2 * HOUR]],
  ])("refuses a row whose times are %s", (_, time) => {
    expect(ask({ ...complete, completed_at: time })).toEqual(UNKNOWN_ROW);
    expect(ask({ ...complete, checked_at: time })).toEqual(UNKNOWN_ROW);
  });

  it("refuses a completed row whose keys were never checked", () => {
    expect(ask({ ...complete, checked_at: null })).toEqual(UNKNOWN_ROW);
  });

  it.each([[7], [""], [true], [{}]])("refuses a row whose rotation is %j", (rotation) => {
    expect(ask({ ...complete, rotation })).toEqual(UNKNOWN_ROW);
  });

  it.each([["a string", JSON.stringify(complete)], ["a number", 1], ["a boolean", true]])("refuses a row that is %s", (_, row) => {
    expect(ask(row)).toEqual(UNKNOWN_ROW);
  });

  it("refuses a list of rows where one row belongs", () => {
    expect(ask([complete])).toEqual(UNKNOWN_ROW);
    expect(ask([])).toEqual(UNKNOWN_ROW);
  });
});

/** D1's interface over an in-memory SQLite with the gateway's own migrations for the table. */
function sweepDatabase(): { db: D1Database; row: () => unknown } {
  const database = new DatabaseSync(":memory:");
  for (const file of ["0013_secrets_rotation.sql", "0014_secrets_rotation_total.sql"]) {
    database.exec(readFileSync(join(HERE, "../migrations", file), "utf8"));
  }
  const statement = (sql: string, args: SQLInputValue[]) => ({
    bind: (...bound: SQLInputValue[]) => statement(sql, bound),
    first: async () => database.prepare(sql).get(...args) ?? null,
    run: async () => database.prepare(sql).run(...args),
  });
  return {
    db: { prepare: (sql: string) => statement(sql, []) } as unknown as D1Database,
    // Through JSON, as the script gets it from wrangler.
    row: () => JSON.parse(JSON.stringify(database.prepare("SELECT * FROM secrets_rotation").get() ?? null)) ?? undefined,
  };
}

describe("the verdict on the row the sweep stores", () => {
  it("follows a rotation from no row, through its passes, to the previous key being dropped", async () => {
    const { db, row } = sweepDatabase();
    const store = { ...d1SweepStore(db), projects: async (after: string) => ["p1"].filter((id) => id > after) };
    const api: Reseal = { rotation: "r1", slot: "2", resealed: 4, unreadable: 1 };
    // `start` at NOW moved the slot to 2; the deploy landed before the first cron run, 20 minutes on.
    const facts = { current: 2, changedAt: NOW };
    const run = (at: number) => sweep(store, async () => api, 5, at);
    const first = NOW + HOUR / 3;
    expect(ask(row(), facts)).toEqual(NO_PASS);

    // The first pass reseals, so it does not complete; the second reseals nothing and does.
    await run(first);
    expect(row()).toMatchObject({ rotation: "r1", slot: "2", completed_at: null, resealed_total: 4 });
    expect(ask(row(), facts)).toEqual(NO_PASS);
    api.resealed = 0;
    const completed = first + HOUR;
    await run(completed);
    expect(row()).toMatchObject({ rotation: "r1", slot: "2", completed_at: completed, checked_at: completed, unreadable: 1 });
    expect(ask(row(), facts)).toEqual(TOO_SOON);

    // Complete: each run only probes, and the probe moves checked_at. The next run started 700 ms
    // sooner into its minute, so it is under an hour after the completion; the one after decides.
    await run(completed + HOUR - 700);
    expect(row()).toMatchObject({ completed_at: completed, checked_at: completed + HOUR - 700 });
    expect(ask(row(), facts)).toEqual(TOO_SOON);
    await run(completed + 2 * HOUR - 700);
    expect(ask(row(), facts)).toEqual(FINISH);

    // Finished and deployed: the API has no previous key, and the row is no longer a completed pass.
    api.rotation = null;
    await run(completed + 3 * HOUR);
    expect(ask(row(), facts)).toEqual(NO_PASS);
  });

  it("refuses after two undos with a deploy in flight, until the sweep has seen what that deploy brought", async () => {
    const { db, row } = sweepDatabase();
    const store = { ...d1SweepStore(db), projects: async (after: string) => ["p1"].filter((id) => id > after) };
    const api: Reseal = { rotation: "r1", slot: "2", resealed: 0, unreadable: 0 };
    const run = (at: number) => sweep(store, async () => api, 5, at);
    await run(NOW);
    await run(NOW + HOUR);
    expect(ask(row(), { changedAt: NOW - HOUR })).toEqual(FINISH);

    // `undo` (slot 1), a deploy starts and reads slot 1, `undo` again (slot 2), and the sweep probes
    // before that deploy lands: the keys it finds are still slot 2's, checked after the last change,
    // on a pass completed two hours ago.
    const undoneTwice = { current: 2, changedAt: NOW + 2 * HOUR - 60_000 };
    await run(NOW + 2 * HOUR);
    expect(row()).toMatchObject({ slot: "2", completed_at: NOW, checked_at: NOW + 2 * HOUR });
    expect(ask(row(), undoneTwice)).toEqual(NOT_CHECKED);

    // The deploy lands and seals new values under slot 1's key, the one `finish` would delete.
    // By the time a check is an hour after the change, the sweep has seen those keys.
    api.rotation = "r2";
    api.slot = "1";
    await run(NOW + 3 * HOUR);
    expect(ask(row(), undoneTwice)).toEqual(NO_PASS);
    await run(NOW + 4 * HOUR);
    await run(NOW + 5 * HOUR);
    expect(ask(row(), undoneTwice)).toEqual(otherSlot("1", 2));
  });
});

const SCRIPT = join(HERE, "../../../scripts/rotate-secrets-key.mjs");

/**
 * Runs the script's `finish` with stand-ins for `gh` and `npx` as the only programs on PATH: they
 * answer the script's reads from files and record its writes, so nothing real can be reached.
 */
function finish(variables: unknown[], row: unknown): { status: number | null; stderr: string; writes: string[] } {
  const bin = mkdtempSync(join(tmpdir(), "finish-verdict-"));
  const program = (name: string, body: string) => {
    writeFileSync(join(bin, name), `#!/bin/sh\n${body}\n`);
    chmodSync(join(bin, name), 0o755);
  };
  program(
    "gh",
    'case "$1 $2" in\n"variable list") /bin/cat "${0%/*}/variables.json" ;;\n"secret list") /bin/cat "${0%/*}/secrets.json" ;;\n' +
      '*) echo "$1 $2 $3" >> "${0%/*}/writes" ;;\nesac',
  );
  program("npx", '/bin/cat "${0%/*}/d1.json"');
  writeFileSync(join(bin, "variables.json"), JSON.stringify(variables));
  writeFileSync(join(bin, "secrets.json"), JSON.stringify([{ name: "SECRETS_KEY" }, { name: "SECRETS_KEY_2" }]));
  writeFileSync(join(bin, "d1.json"), JSON.stringify([{ results: row === undefined ? [] : [row], success: true }]));
  const { status, stderr } = spawnSync(process.execPath, [SCRIPT, "--env", "staging", "finish"], {
    env: { PATH: bin, CLOUDFLARE_ACCOUNT_ID: "none" },
    encoding: "utf8",
  });
  const log = join(bin, "writes");
  const writes = existsSync(log) ? readFileSync(log, "utf8").trim().split("\n") : [];
  rmSync(bin, { recursive: true });
  return { status, stderr, writes };
}

describe("rotate-secrets-key.mjs finish", () => {
  const started = Date.now();
  const ago = (ms: number) => started - ms;
  const variable = (name: string, value: string, changed = ago(5 * HOUR)) => ({ name, value, updatedAt: new Date(changed).toISOString() });
  const rotatingOn2 = [variable("SECRETS_KEY_SLOT", "2"), variable("SECRETS_KEY_ROTATING", "true"), variable("API_URL", "x", ago(0))];
  const completed = () => ({ ...complete, completed_at: ago(2 * HOUR), checked_at: ago(HOUR) });
  const refused = (why: string) => ({ status: 1, stderr: `staging: ${why}\n`, writes: [] });

  it("ends the rotation and deletes the other slot when the verdict is to finish", () => {
    expect(finish(rotatingOn2, completed())).toMatchObject({
      status: 0,
      writes: ["variable set SECRETS_KEY_ROTATING", "secret delete SECRETS_KEY"],
    });
  });

  it("writes nothing when the verdict refuses", () => {
    expect(finish(rotatingOn2, undefined)).toEqual(refused(NO_PASS.why));
    expect(finish(rotatingOn2, { ...completed(), slot: "1" })).toEqual(refused(otherSlot("1", 2).why));
    expect(finish(rotatingOn2, { ...completed(), completed_at: ago(HOUR / 2), checked_at: ago(0) })).toEqual(refused(TOO_SOON.why));
    expect(finish(rotatingOn2, without("completed_at"))).toEqual(refused(UNKNOWN_ROW.why));
  });

  it("is not moved by the laptop's clock, a day slow or a day fast", () => {
    const DAY = 24 * HOUR;
    const rotating = (changed: number) => [variable("SECRETS_KEY_SLOT", "2", changed), variable("SECRETS_KEY_ROTATING", "true", changed)];
    // Slow: GitHub and the gateway are a day ahead of it, and the pass is good.
    const ahead = { ...complete, completed_at: ago(2 * HOUR - DAY), checked_at: ago(HOUR - DAY) };
    expect(finish(rotating(ago(5 * HOUR - DAY)), ahead)).toMatchObject({ status: 0 });
    // Fast: the pass looks a day old from here, but the sweep last checked half an hour after it completed.
    const behind = { ...complete, completed_at: ago(HOUR / 2 + DAY), checked_at: ago(DAY) };
    expect(finish(rotating(ago(5 * HOUR + DAY)), behind)).toEqual(refused(TOO_SOON.why));
  });

  it("takes the last change from the newest SECRETS_KEY_ variable", () => {
    const undone = [variable("SECRETS_KEY_SLOT", "2", ago(HOUR / 2)), variable("SECRETS_KEY_ROTATING", "true")];
    expect(finish(undone, completed())).toEqual(refused(NOT_CHECKED.why));
  });

  it("refuses when a variable's time of change can't be read", () => {
    const undated = [{ name: "SECRETS_KEY_SLOT", value: "2" }, variable("SECRETS_KEY_ROTATING", "true")];
    expect(finish(undated, completed())).toEqual(refused(UNKNOWN_CHANGE.why));
    const misdated = [{ name: "SECRETS_KEY_SLOT", value: "2", updatedAt: "yesterday" }, variable("SECRETS_KEY_ROTATING", "true")];
    expect(finish(misdated, completed())).toEqual(refused(UNKNOWN_CHANGE.why));
  });

  it("reads the slot as the deploy does: anything but 2 is slot 1", () => {
    for (const slot of [[], [variable("SECRETS_KEY_SLOT", "1")], [variable("SECRETS_KEY_SLOT", " 2")], [variable("SECRETS_KEY_SLOT", "3")]]) {
      expect(finish([...slot, variable("SECRETS_KEY_ROTATING", "true")], completed())).toEqual(refused(otherSlot("2", 1).why));
    }
  });

  it("has nothing to finish unless SECRETS_KEY_ROTATING is exactly true", () => {
    for (const rotating of [[], [variable("SECRETS_KEY_ROTATING", "false")], [variable("SECRETS_KEY_ROTATING", "TRUE")]]) {
      expect(finish([variable("SECRETS_KEY_SLOT", "2"), ...rotating], completed())).toEqual(refused("no rotation to finish"));
    }
  });
});
