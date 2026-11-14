// scripts/rotate-secrets-key.mjs: the verdict it asks before `finish` deletes the previous
// SECRETS_KEY, then the script itself, each step run against stand-ins for `gh` and `npx`.
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

const BOTH = ["SECRETS_KEY", "SECRETS_KEY_2"];
const started = Date.now();
const ago = (ms: number) => started - ms;
const variable = (name: string, value: string, changed = ago(5 * HOUR)) => ({ name, value, updatedAt: new Date(changed).toISOString() });

/**
 * Runs the script with stand-ins for `gh` and `npx` as the only programs on PATH: they answer the
 * script's reads from files and record its writes, so nothing real can be reached. `fail` names a
 * write (`variable set SECRETS_KEY_SLOT`) that `gh` refuses, as GitHub does when a call fails.
 * `writes` are the `gh` command lines, less the `--repo` each one ends with; `key` is what the
 * script piped to `gh secret set`.
 */
function rotate(args: string[], { variables = [] as unknown[], secrets = BOTH, row = undefined as unknown, fail = "" } = {}) {
  const bin = mkdtempSync(join(tmpdir(), "rotate-secrets-key-"));
  const program = (name: string, lines: string[]) => {
    writeFileSync(join(bin, name), `#!/bin/sh\ndir="\${0%/*}"\n${lines.join("\n")}\n`);
    chmodSync(join(bin, name), 0o755);
  };
  program("gh", [
    'case "$1 $2" in',
    '"variable list") /bin/cat "$dir/variables.json" ;;',
    '"secret list") /bin/cat "$dir/secrets.json" ;;',
    "*)",
    '  if [ "$1 $2" = "secret set" ]; then /bin/cat > "$dir/key"; fi',
    '  read -r fail < "$dir/fail"',
    '  if [ "$1 $2 $3" = "$fail" ]; then echo "HTTP 403: Resource not accessible" >&2; exit 1; fi',
    '  echo "$*" >> "$dir/writes" ;;',
    "esac",
  ]);
  program("npx", ['/bin/cat "$dir/d1.json"']);
  writeFileSync(join(bin, "variables.json"), JSON.stringify(variables));
  writeFileSync(join(bin, "secrets.json"), JSON.stringify(secrets.map((name) => ({ name }))));
  writeFileSync(join(bin, "d1.json"), JSON.stringify([{ results: row === undefined ? [] : [row], success: true }]));
  writeFileSync(join(bin, "fail"), `${fail}\n`);
  const { status, stdout, stderr } = spawnSync(process.execPath, [SCRIPT, ...args], {
    env: { PATH: bin, CLOUDFLARE_ACCOUNT_ID: "none" },
    encoding: "utf8",
  });
  const read = (name: string) => (existsSync(join(bin, name)) ? readFileSync(join(bin, name), "utf8") : undefined);
  const lines = read("writes")?.trim().split("\n") ?? [];
  const key = read("key");
  rmSync(bin, { recursive: true });
  expect(lines.filter((line) => !line.endsWith(" --repo thachp/agentlater"))).toEqual([]);
  return { status, stdout, stderr, key, writes: lines.map((line) => line.slice(0, line.lastIndexOf(" --repo "))) };
}

const refused = (why: string) => ({ status: 1, stderr: `staging: ${why}\n`, writes: [] });
const unknownValue = (name: string, value: string, known: string) =>
  refused(`${name} is ${JSON.stringify(value)}, not ${known}; see docs/DEPLOY.md, Rotating SECRETS_KEY`);
const SET_KEY = (slot: string) => `secret set ${slot} --env staging`;
const SET = (name: string, value: string) => `variable set ${name} --env staging --body ${value}`;

/** The environment as GitHub holds it: its variables, and the key in each slot that is set. */
type Held = { variables: Record<string, string>; keys: Record<string, string> };

/** Before any rotation, or after a finished one that left `slot` current. */
const settled = (slot: 1 | 2): Held =>
  slot === 1
    ? { variables: {}, keys: { SECRETS_KEY: "the old key" } }
    : { variables: { SECRETS_KEY_SLOT: "2", SECRETS_KEY_ROTATING: "false" }, keys: { SECRETS_KEY_2: "the old key" } };

const step = (name: string, { variables, keys }: Held, fail = "") =>
  rotate(["--env", "staging", name], {
    variables: Object.entries(variables).map(([n, value]) => variable(n, value)),
    secrets: Object.keys(keys),
    fail,
  });

/** `held` after one of the script's writes. */
function after(held: Held, write: string, key: string | undefined): Held {
  const [, verb, name = "", body] = /^(secret set|secret delete|variable set) (\S+) --env staging(?: --body (\S+))?$/.exec(write) ?? [];
  const { variables, keys } = structuredClone(held);
  if (verb === "variable set" && body) variables[name] = body;
  else if (verb === "secret set" && key) keys[name] = key;
  else if (verb === "secret delete") delete keys[name];
  else throw new Error(`not a write this test knows: ${write}`);
  return { variables, keys };
}

const WORKFLOW = readFileSync(join(HERE, "../../../.github/workflows/deploy-env.yml"), "utf8");
/** The lines of deploy-env.yml's "Deploy api" step that `deployed` below stands for. */
const DEPLOY_READS = [
  "SLOT_1: ${{ secrets.SECRETS_KEY }}",
  "SLOT_2: ${{ secrets.SECRETS_KEY_2 }}",
  "ROTATING: ${{ vars.SECRETS_KEY_ROTATING == 'true' }}",
  "CURRENT_SLOT: ${{ vars.SECRETS_KEY_SLOT == '2' && '2' || '1' }}",
  'if [ "$CURRENT_SLOT" = 2 ]; then SECRETS_KEY=$SLOT_2 SECRETS_KEY_PREVIOUS=$SLOT_1; else SECRETS_KEY=$SLOT_1 SECRETS_KEY_PREVIOUS=$SLOT_2; fi',
  'if [ "$ROTATING" != true ]; then SECRETS_KEY_PREVIOUS=; fi',
  'if [ "$ROTATING" = true ] && { [ -z "$SECRETS_KEY" ] || [ -z "$SECRETS_KEY_PREVIOUS" ]; }; then',
];

/**
 * The keys a deploy hands the API, the current one first, or "fails" when the deploy stops before
 * it deploys. GitHub's `==` compares two strings ignoring case and nothing else, so TRUE is a
 * rotation and only "2" is slot 2.
 */
function deployed({ variables, keys }: Held): string[] | "fails" {
  const rotating = (variables.SECRETS_KEY_ROTATING ?? "").toLowerCase() === "true";
  const [current, previous] = variables.SECRETS_KEY_SLOT === "2" ? [keys.SECRETS_KEY_2, keys.SECRETS_KEY] : [keys.SECRETS_KEY, keys.SECRETS_KEY_2];
  if (rotating && !(current && previous)) return "fails";
  return [current, rotating ? previous : undefined].filter((key) => key !== undefined);
}

/**
 * Runs the steps one after another with a deploy after every single write, and checks that each
 * deploy hands the API every key in use: every key an earlier deploy handed it, since a value may
 * be sealed under any key that was once current, and the previous key is there because one is.
 * Deploying after every write is the hardest case: each deploy only adds to the keys in use.
 */
function keepsEveryKey(held: Held, steps: string[], fail = ""): Held {
  const inUse = new Set<string>();
  const deploy = (when: string) => {
    const handed = deployed(held);
    expect(handed, when).toEqual(expect.arrayContaining([...inUse]));
    for (const key of handed) inUse.add(key);
  };
  deploy("at the start");
  for (const name of steps) {
    const { writes, key } = step(name, held, fail);
    for (const write of writes) {
      held = after(held, write, key);
      deploy(`after ${write}`);
    }
  }
  return held;
}

describe("rotate-secrets-key.mjs start and undo", () => {
  const rotating = (slot: 1 | 2): Held => ({
    variables: { SECRETS_KEY_SLOT: String(slot), SECRETS_KEY_ROTATING: "true" },
    keys: { SECRETS_KEY: "one key", SECRETS_KEY_2: "another key" },
  });

  it("models the deploy from the lines deploy-env.yml has", () => {
    for (const line of DEPLOY_READS) expect(WORKFLOW).toContain(`\n          ${line}\n`);
    const keys = { SECRETS_KEY: "k1", SECRETS_KEY_2: "k2" };
    const reads = (variables: Record<string, string>) => deployed({ variables, keys });
    expect(reads({})).toEqual(["k1"]);
    expect(reads({ SECRETS_KEY_SLOT: "2" })).toEqual(["k2"]);
    expect(reads({ SECRETS_KEY_SLOT: "2", SECRETS_KEY_ROTATING: "true" })).toEqual(["k2", "k1"]);
    expect(reads({ SECRETS_KEY_SLOT: "1", SECRETS_KEY_ROTATING: "TRUE" })).toEqual(["k1", "k2"]);
    expect(reads({ SECRETS_KEY_SLOT: " 2", SECRETS_KEY_ROTATING: "false" })).toEqual(["k1"]);
    expect(deployed({ variables: { SECRETS_KEY_ROTATING: "true" }, keys: { SECRETS_KEY: "k1" } })).toBe("fails");
  });

  it("start writes the new key, then the flag, then the slot", () => {
    expect(step("start", settled(1))).toMatchObject({
      status: 0,
      writes: [SET_KEY("SECRETS_KEY_2"), SET("SECRETS_KEY_ROTATING", "true"), SET("SECRETS_KEY_SLOT", "2")],
      stdout: "staging: new key in SECRETS_KEY_2, now current; SECRETS_KEY opens until finish. Deploy now.\n",
    });
    expect(step("start", settled(2))).toMatchObject({
      status: 0,
      writes: [SET_KEY("SECRETS_KEY"), SET("SECRETS_KEY_ROTATING", "true"), SET("SECRETS_KEY_SLOT", "1")],
    });
  });

  it("writes to the environment it was given", () => {
    const { writes } = rotate(["--env", "production", "start"], { secrets: ["SECRETS_KEY"] });
    expect(writes).toHaveLength(3);
    for (const write of writes) expect(write).toContain(" --env production");
  });

  it("undo only moves the slot back", () => {
    expect(step("undo", rotating(2))).toMatchObject({
      status: 0,
      writes: [SET("SECRETS_KEY_SLOT", "1")],
      stdout: "staging: SECRETS_KEY is current again; SECRETS_KEY_2 still opens. Deploy now, then finish to drop it.\n",
    });
    expect(step("undo", rotating(1))).toMatchObject({ status: 0, writes: [SET("SECRETS_KEY_SLOT", "2")] });
  });

  it.each([1, 2] as const)("from slot %i, a deploy after any write of start, undo and undo again hands the API every key in use", (slot) => {
    const end = keepsEveryKey(settled(slot), ["start", "undo", "undo"]);
    expect(end.variables).toEqual({ SECRETS_KEY_SLOT: String(3 - slot), SECRETS_KEY_ROTATING: "true" });
    expect(deployed(end)).toEqual([end.keys[slot === 1 ? "SECRETS_KEY_2" : "SECRETS_KEY"], "the old key"]);
  });

  it("the order is what keeps them: the slot before the flag would hand the API the new key alone", () => {
    const { writes, key } = step("start", settled(1));
    const [setKey = "", flag = "", slot = ""] = writes;
    const apply = (order: string[]) => deployed(order.reduce((held, write) => after(held, write, key), settled(1)));
    expect(apply([setKey, slot])).toEqual([key]);
    // The flag before the key: the deploy fails rather than drop a key, but nothing can ship.
    expect(apply([flag])).toBe("fails");
    expect(apply([setKey, flag])).toEqual(["the old key", key]);
  });

  it.each([
    // Nothing was written: a second start is the first.
    { failing: "secret set SECRETS_KEY_2", left: 0, next: { start: 3, undo: 0 } },
    // The new key is in its slot and no deploy passes it: a second start replaces it, unused.
    { failing: "variable set SECRETS_KEY_ROTATING", left: 1, next: { start: 3, undo: 0 } },
    // Rotating with the old key still current: start refuses, and undo moves the slot on to the new key.
    { failing: "variable set SECRETS_KEY_SLOT", left: 2, next: { start: 0, undo: 1 } },
  ])("a start that fails at $failing leaves $left writes, and a deploy after any later write keeps every key", ({ failing, left, next }) => {
    const failed = step("start", settled(1), failing);
    expect(failed.status).not.toBe(0);
    expect(failed.stderr).toContain("HTTP 403");
    expect(failed.writes).toEqual([SET_KEY("SECRETS_KEY_2"), SET("SECRETS_KEY_ROTATING", "true")].slice(0, left));
    const stopped = keepsEveryKey(settled(1), ["start"], failing);
    for (const [name, written] of Object.entries(next)) {
      expect(step(name, stopped).writes).toHaveLength(written);
      keepsEveryKey(stopped, [name, "undo", "undo"]);
    }
  });

  it("start refuses while a rotation is on", () => {
    for (const slot of [1, 2] as const) expect(step("start", rotating(slot))).toMatchObject(refused("a rotation is already on; finish or undo it first"));
  });

  it("undo refuses when no rotation is on, or the other slot has no key", () => {
    const none = refused("no rotation to undo");
    expect(step("undo", settled(1))).toMatchObject(none);
    expect(step("undo", settled(2))).toMatchObject(none);
    expect(step("undo", { ...rotating(1), variables: { SECRETS_KEY_ROTATING: "false" } })).toMatchObject(none);
    expect(step("undo", { ...rotating(2), keys: { SECRETS_KEY_2: "the only key" } })).toMatchObject(none);
  });

  it("undo refuses after finish, whether or not finish got to delete the other slot", () => {
    const finished = { SECRETS_KEY_SLOT: "2", SECRETS_KEY_ROTATING: "false" };
    expect(step("undo", { variables: finished, keys: { SECRETS_KEY_2: "the new key" } })).toMatchObject(refused("no rotation to undo"));
    const undeleted = { variables: finished, keys: { SECRETS_KEY: "the old key", SECRETS_KEY_2: "the new key" } };
    expect(step("undo", undeleted)).toMatchObject(refused("no rotation to undo"));
    // The deploy passes only the current key by then, so a new start may take the slot finish left.
    expect(keepsEveryKey(undeleted, ["start"]).variables).toEqual({ SECRETS_KEY_SLOT: "1", SECRETS_KEY_ROTATING: "true" });
  });

  it.each(["start", "undo", "finish"])("%s refuses when the current slot has no key", (name) => {
    const rotation = [variable("SECRETS_KEY_ROTATING", "true")];
    const on = (variables: unknown[], secrets: string[]) => rotate(["--env", "staging", name], { variables, secrets });
    expect(on(rotation, [])).toMatchObject(refused("SECRETS_KEY (the current key) is not set; nothing to rotate"));
    expect(on(rotation, ["SECRETS_KEY_2"])).toMatchObject(refused("SECRETS_KEY (the current key) is not set; nothing to rotate"));
    expect(on([variable("SECRETS_KEY_SLOT", "2"), ...rotation], ["SECRETS_KEY"])).toMatchObject(
      refused("SECRETS_KEY_2 (the current key) is not set; nothing to rotate"),
    );
  });

  describe("refuses a value it does not write, which the deploy may read another way", () => {
    const on = (name: string, variables: unknown[]) => rotate(["--env", "staging", name], { variables, row: complete });

    // The deploy reads the first two as a rotation, and passes the other slot as the previous key.
    it.each(["TRUE", "True", " true", "1", "yes", "FALSE", ""])("start with SECRETS_KEY_ROTATING=%j", (value) => {
      const variables = [variable("SECRETS_KEY_SLOT", "2"), variable("SECRETS_KEY_ROTATING", value)];
      expect(on("start", variables)).toMatchObject(unknownValue("SECRETS_KEY_ROTATING", value, "false or true"));
    });

    // The deploy reads every one of these as slot 1.
    it.each(["3", "0", " 2", "02", "two", ""])("start with SECRETS_KEY_SLOT=%j", (value) => {
      const variables = [variable("SECRETS_KEY_SLOT", value), variable("SECRETS_KEY_ROTATING", "false")];
      expect(on("start", variables)).toMatchObject(unknownValue("SECRETS_KEY_SLOT", value, "1 or 2"));
    });

    it.each(["undo", "finish"])("%s too", (name) => {
      expect(on(name, [variable("SECRETS_KEY_ROTATING", "TRUE")])).toMatchObject(unknownValue("SECRETS_KEY_ROTATING", "TRUE", "false or true"));
      const slot = [variable("SECRETS_KEY_SLOT", " 2"), variable("SECRETS_KEY_ROTATING", "true")];
      expect(on(name, slot)).toMatchObject(unknownValue("SECRETS_KEY_SLOT", " 2", "1 or 2"));
    });
  });

  it("with SECRETS_KEY_ROTATING=TRUE set by hand, start does not replace the key the deploy passes as previous", () => {
    const byHand = { ...rotating(1), variables: { SECRETS_KEY_ROTATING: "TRUE" } };
    expect(deployed(byHand)).toEqual(["one key", "another key"]);
    expect(keepsEveryKey(byHand, ["start", "undo"])).toEqual(byHand);
  });

  it.each([
    [[]],
    [["start"]],
    [["--env", "staging"]],
    [["--env", "start"]],
    [["--env", "dev", "start"]],
    [["--env", "staging", "restart"]],
    [["--env", "staging", "start", "undo"]],
    [["--env", "staging", "undo", "--force"]],
    [["--env", "staging", "--env", "production", "start"]],
    [["start", "--env", "staging"]],
    [["--env=staging", "start"]],
  ])("refuses the arguments %j before it reads anything", (args) => {
    // Had it read the secrets, it would have refused for the missing key instead.
    expect(rotate(args, { secrets: [] })).toMatchObject({
      status: 2,
      stderr: "usage: node scripts/rotate-secrets-key.mjs --env staging|production start|undo|finish\n",
      stdout: "",
      writes: [],
    });
  });

  it("prints nothing secret: the key goes to gh on stdin, a new one each time, and into no argument or output", () => {
    const runs = [step("start", settled(1)), step("start", settled(1)), step("start", settled(1), "secret set SECRETS_KEY_2")];
    for (const { key, stdout, stderr, writes } of runs) {
      expect(key).toMatch(/^[0-9a-f]{64}$/);
      expect([stdout, stderr, ...writes].join("\n")).not.toContain(key);
    }
    expect(new Set(runs.map((run) => run.key)).size).toBe(3);
  });
});

const finish = (variables: unknown[], row: unknown) => {
  const { status, stderr, writes } = rotate(["--env", "staging", "finish"], { variables, row });
  return { status, stderr, writes };
};

describe("rotate-secrets-key.mjs finish", () => {
  const rotatingOn2 = [variable("SECRETS_KEY_SLOT", "2"), variable("SECRETS_KEY_ROTATING", "true"), variable("API_URL", "x", ago(0))];
  const completed = () => ({ ...complete, completed_at: ago(2 * HOUR), checked_at: ago(HOUR) });

  it("ends the rotation and deletes the other slot when the verdict is to finish", () => {
    expect(finish(rotatingOn2, completed())).toMatchObject({
      status: 0,
      writes: [SET("SECRETS_KEY_ROTATING", "false"), "secret delete SECRETS_KEY --env staging"],
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

  it("reads an unset slot, or 1, as slot 1", () => {
    for (const slot of [[], [variable("SECRETS_KEY_SLOT", "1")]]) {
      expect(finish([...slot, variable("SECRETS_KEY_ROTATING", "true")], completed())).toEqual(refused(otherSlot("2", 1).why));
    }
  });

  it("has nothing to finish when SECRETS_KEY_ROTATING is unset or false", () => {
    for (const rotating of [[], [variable("SECRETS_KEY_ROTATING", "false")]]) {
      expect(finish([variable("SECRETS_KEY_SLOT", "2"), ...rotating], completed())).toEqual(refused("no rotation to finish"));
    }
  });
});
