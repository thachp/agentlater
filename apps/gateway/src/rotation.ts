// Rotating SECRETS_KEY (docs/adr/0001-rotating-secrets-key.md). While the API has a previous key,
// the hourly cron walks every project and asks its ledger to reseal, under the current key, what
// only the previous key opens. A full pass that reseals nothing marks the rotation complete, and
// only then can the previous key be dropped (docs/DEPLOY.md, "Rotating SECRETS_KEY").

import { ApiClient } from "./api";

/** Projects asked per cron run, well inside a Worker's subrequest limit. */
const PROJECTS_PER_RUN = 200;

/** Where the sweep is: which rotation, the last project done in this pass, and what the pass found. */
export interface SweepState {
  /** The API's name for its pair of keys; null when no rotation is on. */
  rotation: string | null;
  /** The GitHub slot the deploy took the current key from (`SECRETS_KEY_SLOT`). */
  slot: string | null;
  cursor: string;
  /** Resealed in this pass; a pass that reseals nothing completes the rotation. */
  resealed: number;
  /** Resealed over the whole rotation, every pass: what the rotation actually moved. */
  total: number;
  unreadable: number;
  /** When a full pass resealed nothing: the previous key is no longer needed. */
  completedAt: number | null;
  /** The last time a ledger answered with these keys. */
  checkedAt: number | null;
}

export const FRESH: SweepState = {
  rotation: null,
  slot: null,
  cursor: "",
  resealed: 0,
  total: 0,
  unreadable: 0,
  completedAt: null,
  checkedAt: null,
};

/** One ledger's answer. `rotation` is null when the API has no previous key. */
export interface Reseal {
  rotation: string | null;
  slot: string | null;
  resealed: number;
  unreadable: number;
}

export interface SweepStore {
  load(): Promise<SweepState>;
  save(state: SweepState): Promise<void>;
  /** Project ids after `after`, in order. */
  projects(after: string, limit: number): Promise<string[]>;
}

/**
 * One cron run. New keys (a new rotation, or an undo) start a fresh pass; no previous key resets
 * the sweep; a complete rotation is only probed, which keeps `checkedAt` current. A failed call
 * ends the run where it is, and the next run carries on from there.
 */
export async function sweep(
  store: SweepStore,
  reseal: (projectId: string) => Promise<Reseal>,
  limit = PROJECTS_PER_RUN,
  now = Date.now(),
): Promise<SweepState> {
  const loaded = await store.load();
  let state = loaded;
  const restart = (r: Reseal): SweepState => (r.rotation === null ? FRESH : { ...FRESH, rotation: r.rotation, slot: r.slot });
  const sameKeys = (r: Reseal) => r.rotation === state.rotation && r.slot === state.slot;
  try {
    const page = await store.projects(state.cursor, limit);
    // The first project of the page also answers which rotation, if any, is on.
    const probe = page[0] ?? (await store.projects("", 1))[0];
    if (probe === undefined) return state;
    if (state.completedAt !== null) {
      const r = await reseal(probe);
      return (state = sameKeys(r) ? { ...state, checkedAt: now } : restart(r));
    }
    // A pass can end on an empty page; one project is still asked, so the end is checked
    // against the keys deployed now, not the ones the pass started with.
    for (const projectId of page.length > 0 ? page : [probe]) {
      const r = await reseal(projectId);
      if (r.rotation === null) return (state = FRESH);
      if (!sameKeys(r)) {
        // Keys changed mid-pass: start over from the first project next run.
        if (state.cursor !== "") return (state = restart(r));
        // At the start of a pass: carry on, but nothing found under the old keys counts toward these.
        state = restart(r);
      }
      state = {
        ...state,
        cursor: page.length > 0 ? projectId : state.cursor,
        resealed: state.resealed + r.resealed,
        total: state.total + r.resealed,
        unreadable: state.unreadable + r.unreadable,
        checkedAt: now,
      };
    }
    if (page.length < limit) {
      // End of a pass: complete if it resealed nothing (values no key opens can't be helped by
      // keeping the old one), else go round again to confirm.
      const { rotation, slot, total, unreadable } = state;
      state =
        state.resealed === 0
          ? { ...FRESH, rotation, slot, total, unreadable, completedAt: now, checkedAt: now }
          : { ...FRESH, rotation, slot, total };
    }
    return state;
  } finally {
    if (JSON.stringify(state) !== JSON.stringify(loaded)) await store.save(state);
  }
}

/** The sweep's state in D1 (`secrets_rotation`, one row) and the project list. */
export function d1SweepStore(db: D1Database): SweepStore {
  return {
    async load() {
      const row = await db
        .prepare("SELECT rotation, slot, cursor, resealed, resealed_total, unreadable, completed_at, checked_at FROM secrets_rotation WHERE id = 1")
        .first<{
          rotation: string | null;
          slot: string | null;
          cursor: string;
          resealed: number;
          resealed_total: number;
          unreadable: number;
          completed_at: number | null;
          checked_at: number | null;
        }>();
      return row
        ? {
            rotation: row.rotation,
            slot: row.slot,
            cursor: row.cursor,
            resealed: row.resealed,
            total: row.resealed_total,
            unreadable: row.unreadable,
            completedAt: row.completed_at,
            checkedAt: row.checked_at,
          }
        : FRESH;
    },
    async save(s) {
      await db
        .prepare(
          "INSERT INTO secrets_rotation (id, rotation, slot, cursor, resealed, resealed_total, unreadable, completed_at, checked_at) " +
            "VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT (id) DO UPDATE SET rotation = excluded.rotation, " +
            "slot = excluded.slot, cursor = excluded.cursor, resealed = excluded.resealed, " +
            "resealed_total = excluded.resealed_total, " +
            "unreadable = excluded.unreadable, completed_at = excluded.completed_at, checked_at = excluded.checked_at",
        )
        .bind(s.rotation, s.slot, s.cursor, s.resealed, s.total, s.unreadable, s.completedAt, s.checkedAt)
        .run();
    },
    async projects(after, limit) {
      const { results } = await db
        .prepare("SELECT project_id FROM runlater_project WHERE project_id > ? ORDER BY project_id LIMIT ?")
        .bind(after, limit)
        .all<{ project_id: string }>();
      return results.map((r) => r.project_id);
    },
  };
}

/** Asks one project's ledger to reseal, with the gateway-only `maintain` scope. */
export function resealOver(api: Fetcher, key: CryptoKey): (projectId: string) => Promise<Reseal> {
  return (projectId) => new ApiClient(api, key, projectId).call<Reseal>("POST", "/v1/secrets/_reseal", "maintain");
}
