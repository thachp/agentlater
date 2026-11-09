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

/** A fake API: each project's values reseal once, then are current. */
function api(rotation: string | null, toReseal: Record<string, number> = {}, fails = new Set<string>()) {
  const calls: string[] = [];
  const fake = {
    rotation,
    calls,
    reseal: async (id: string): Promise<Reseal> => {
      calls.push(id);
      if (fails.has(id)) throw new Error("ledger unavailable");
      const resealed = fake.rotation ? (toReseal[id] ?? 0) : 0;
      toReseal[id] = 0;
      return { rotation: fake.rotation, resealed, unreadable: id === "p2" ? 1 : 0 };
    },
  };
  return fake;
}

describe("SECRETS_KEY reseal sweep", () => {
  it("asks one project and resets when no rotation is on", async () => {
    const store = memoryStore(["p1", "p2", "p3"], { ...FRESH, cursor: "p1", resealed: 4 });
    const { reseal, calls } = api(null);
    expect(await sweep(store, reseal, 2, NOW)).toEqual(FRESH);
    expect(calls).toEqual(["p2"]);
  });

  it("walks every project a page per run, and completes on a pass that reseals nothing", async () => {
    const store = memoryStore(["p1", "p2", "p3"]);
    const { reseal, calls } = api("r1", { p1: 2, p3: 1 });

    expect(await sweep(store, reseal, 2, NOW)).toMatchObject({ rotation: "r1", cursor: "p2", resealed: 2, completedAt: null });
    // End of the first pass: it resealed values, so another pass confirms.
    expect(await sweep(store, reseal, 2, NOW)).toEqual({ ...FRESH, rotation: "r1" });
    expect(calls).toEqual(["p1", "p2", "p3"]);

    await sweep(store, reseal, 2, NOW);
    expect(await sweep(store, reseal, 2, NOW)).toEqual({ ...FRESH, rotation: "r1", unreadable: 1, completedAt: NOW });

    // Complete: one probe a run, and nothing else, until the previous key is dropped.
    calls.length = 0;
    expect((await sweep(store, reseal, 2, NOW + 1)).completedAt).toBe(NOW);
    expect(calls).toEqual(["p1"]);
  });

  it("starts over once the previous key is dropped, ready for the next rotation", async () => {
    const store = memoryStore(["p1"], { ...FRESH, rotation: "r1", completedAt: NOW });
    expect(await sweep(store, api(null).reseal, 2, NOW)).toEqual(FRESH);
  });

  it("keeps its place when a ledger fails, and carries on next run", async () => {
    const store = memoryStore(["p1", "p2", "p3"]);
    const fails = new Set(["p2"]);
    const { reseal, calls } = api("r1", { p1: 1, p3: 1 }, fails);
    await expect(sweep(store, reseal, 5, NOW)).rejects.toThrow("ledger unavailable");
    expect(store.state).toMatchObject({ cursor: "p1", resealed: 1 });

    fails.clear();
    expect(await sweep(store, reseal, 5, NOW)).toEqual({ ...FRESH, rotation: "r1" });
    expect(calls).toEqual(["p1", "p2", "p2", "p3"]);
  });

  it("starts a fresh pass when the keys change: a new rotation, or an undo", async () => {
    const fake = api("r1", { p1: 1, p2: 1, p3: 1 });
    const store = memoryStore(["p1", "p2", "p3"], { ...FRESH, rotation: "r1", completedAt: NOW });
    fake.rotation = "r2";
    expect(await sweep(store, fake.reseal, 2, NOW)).toEqual({ ...FRESH, rotation: "r2" });

    // Mid-pass too: what was resealed under r2 may need resealing again under r3.
    await sweep(store, fake.reseal, 2, NOW);
    expect(store.state).toMatchObject({ rotation: "r2", cursor: "p2" });
    fake.rotation = "r3";
    expect(await sweep(store, fake.reseal, 2, NOW)).toEqual({ ...FRESH, rotation: "r3" });
  });

  it("does nothing without projects", async () => {
    const { reseal, calls } = api("r1");
    expect(await sweep(memoryStore([]), reseal, 2, NOW)).toEqual(FRESH);
    expect(calls).toEqual([]);
  });
});
