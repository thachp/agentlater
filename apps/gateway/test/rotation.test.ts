import { describe, expect, it } from "vitest";
import { FRESH, type Reseal, type SweepState, type SweepStore, sweep } from "../src/rotation";

const NOW = 1_800_000_000_000;

function memoryStore(ids: string[], start: SweepState = FRESH): SweepStore & { state: SweepState } {
  const store: SweepStore & { state: SweepState } = {
    state: start,
    async load() {
      return store.state;
    },
    async save(s: SweepState) {
      store.state = s;
    },
    async projects(after: string, limit: number) {
      return ids.filter((id) => id > after).slice(0, limit);
    },
  };
  return store;
}

/** A fake API: each project's values reseal once, then are current. p2 has one unreadable value. */
function api(rotation: string | null, toReseal: Record<string, number> = {}, fails = new Set<string>()) {
  const calls: string[] = [];
  const fake = {
    rotation,
    slot: "2" as string | null,
    calls,
    reseal: async (id: string): Promise<Reseal> => {
      calls.push(id);
      if (fails.has(id)) throw new Error("ledger unavailable");
      const resealed = fake.rotation ? (toReseal[id] ?? 0) : 0;
      toReseal[id] = 0;
      return { rotation: fake.rotation, slot: fake.slot, resealed, unreadable: id === "p2" ? 1 : 0 };
    },
  };
  return fake;
}

const started = (rotation: string, slot = "2"): SweepState => ({ ...FRESH, rotation, slot });

describe("SECRETS_KEY reseal sweep", () => {
  it("asks one project and resets when no rotation is on", async () => {
    const store = memoryStore(["p1", "p2", "p3"], { ...started("r1"), cursor: "p1", resealed: 4 });
    const { reseal, calls } = api(null);
    expect(await sweep(store, reseal, 2, NOW)).toEqual(FRESH);
    expect(calls).toEqual(["p2"]);
  });

  it("walks every project a page per run, and completes on a pass that reseals nothing", async () => {
    const store = memoryStore(["p1", "p2", "p3"]);
    const { reseal, calls } = api("r1", { p1: 2, p3: 1 });

    expect(await sweep(store, reseal, 2, NOW)).toMatchObject({ rotation: "r1", slot: "2", cursor: "p2", resealed: 2, total: 2, completedAt: null });
    // End of the first pass: it resealed values, so another pass confirms. The total carries over.
    expect(await sweep(store, reseal, 2, NOW)).toEqual({ ...started("r1"), total: 3 });
    expect(calls).toEqual(["p1", "p2", "p3"]);

    await sweep(store, reseal, 2, NOW);
    expect(await sweep(store, reseal, 2, NOW)).toEqual({ ...started("r1"), total: 3, unreadable: 1, completedAt: NOW, checkedAt: NOW });

    // Complete: one probe a run, which keeps checkedAt current, until the previous key is dropped.
    calls.length = 0;
    expect(await sweep(store, reseal, 2, NOW + 1)).toMatchObject({ completedAt: NOW, checkedAt: NOW + 1 });
    expect(calls).toEqual(["p1"]);
  });

  it("starts over once the previous key is dropped, ready for the next rotation", async () => {
    const store = memoryStore(["p1"], { ...started("r1"), completedAt: NOW, checkedAt: NOW });
    expect(await sweep(store, api(null).reseal, 2, NOW)).toEqual(FRESH);
  });

  it("starts a fresh pass when the keys change: a new rotation, an undo, or only the slot", async () => {
    const fake = api("r1", { p1: 1, p2: 1, p3: 1 });
    const store = memoryStore(["p1", "p2", "p3"], { ...started("r1"), completedAt: NOW, checkedAt: NOW });
    fake.rotation = "r2";
    expect(await sweep(store, fake.reseal, 2, NOW)).toEqual(started("r2"));

    // Mid-pass too: what was resealed under r2 may need resealing again under r3.
    await sweep(store, fake.reseal, 2, NOW);
    expect(store.state).toMatchObject({ rotation: "r2", cursor: "p2" });
    fake.rotation = "r3";
    expect(await sweep(store, fake.reseal, 2, NOW)).toEqual(started("r3"));

    fake.slot = "1";
    store.state = { ...started("r3"), completedAt: NOW, checkedAt: NOW };
    expect(await sweep(store, fake.reseal, 2, NOW)).toEqual(started("r3", "1"));
  });

  it("counts a new rotation from 0, even when the keys change between passes", async () => {
    // r1's first pass resealed 3 and went round again; then an undo was deployed.
    const fake = api("r2", { p1: 1 });
    const store = memoryStore(["p1", "p2"], { ...started("r1"), total: 3 });
    expect(await sweep(store, fake.reseal, 5, NOW)).toEqual({ ...started("r2"), total: 1 });
  });

  it("checks the end of a pass against the keys deployed now, even on an empty last page", async () => {
    // The last run ended exactly on a full page; the keys changed before the next one.
    const fake = api("r2");
    const store = memoryStore(["p1", "p2"], { ...started("r1"), cursor: "p2" });
    expect(await sweep(store, fake.reseal, 2, NOW)).toEqual(started("r2"));
    expect(fake.calls).toEqual(["p1"]);
  });

  it("keeps its place when a ledger fails, and carries on next run", async () => {
    const store = memoryStore(["p1", "p2", "p3"]);
    const fails = new Set(["p2"]);
    const { reseal, calls } = api("r1", { p1: 1, p3: 1 }, fails);
    await expect(sweep(store, reseal, 5, NOW)).rejects.toThrow("ledger unavailable");
    expect(store.state).toMatchObject({ cursor: "p1", resealed: 1 });

    fails.clear();
    expect(await sweep(store, reseal, 5, NOW)).toEqual({ ...started("r1"), total: 2 });
    expect(calls).toEqual(["p1", "p2", "p2", "p3"]);
  });

  it("does nothing without projects", async () => {
    const { reseal, calls } = api("r1");
    expect(await sweep(memoryStore([]), reseal, 2, NOW)).toEqual(FRESH);
    expect(calls).toEqual([]);
  });
});
