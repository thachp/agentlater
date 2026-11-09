// Rotating SECRETS_KEY (docs/adr/0001-rotating-secrets-key.md). While the API has a previous key,
// the hourly cron walks every project and asks its ledger to reseal, under the current key, what
// only the previous key opens. A full pass that reseals nothing marks the rotation complete, and
// only then can the previous key be dropped (docs/DEPLOY.md, "Rotating SECRETS_KEY").

import { ApiClient } from "./api";

/** Projects asked per cron run, well inside a Worker's subrequest limit. */
export const PROJECTS_PER_RUN = 200;

/** Where the sweep is: which rotation, the last project done in this pass, and what the pass found. */
export interface SweepState {
  /** The API's name for its pair of keys; null when no rotation is on. */
  rotation: string | null;
  cursor: string;
  resealed: number;
  unreadable: number;
  /** When a full pass resealed nothing: the previous key is no longer needed. */
  completedAt: number | null;
}

export const FRESH: SweepState = { rotation: null, cursor: "", resealed: 0, unreadable: 0, completedAt: null };

/** One ledger's answer. `rotation` is null when the API has no previous key. */
export interface Reseal {
  rotation: string | null;
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
 * One cron run. A new pair of keys (a new rotation, or an undo) starts a fresh pass; no previous
 * key resets the sweep; a complete rotation is only probed. A failed call ends the run where it
 * is, and the next run carries on from there.
 */
export async function sweep(
  store: SweepStore,
  reseal: (projectId: string) => Promise<Reseal>,
  limit = PROJECTS_PER_RUN,
  now = Date.now(),
): Promise<SweepState> {
  const loaded = await store.load();
  let state = loaded;
  try {
    const page = await store.projects(state.cursor, limit);
    // The first project of the page also answers which rotation, if any, is on.
    const probe = page[0] ?? (await store.projects("", 1))[0];
    if (probe === undefined) return state;
    if (state.completedAt !== null) {
      const r = await reseal(probe);
      if (r.rotation !== state.rotation) state = { ...FRESH, rotation: r.rotation };
      return state;
    }
    for (const projectId of page) {
      const r = await reseal(projectId);
      if (r.rotation === null) return (state = FRESH);
      if (r.rotation !== state.rotation) {
        // Keys changed mid-pass: start over from the first project next run.
        if (state.cursor !== "") return (state = { ...FRESH, rotation: r.rotation });
        state = { ...state, rotation: r.rotation };
      }
      state = { ...state, cursor: projectId, resealed: state.resealed + r.resealed, unreadable: state.unreadable + r.unreadable };
    }
    if (page.length < limit) {
      // End of a pass: complete if it resealed nothing (values no key opens can't be helped by
      // keeping the old one), else go round again to confirm.
      const { rotation, unreadable } = state;
      state = state.resealed === 0 ? { ...FRESH, rotation, unreadable, completedAt: now } : { ...FRESH, rotation };
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
        .prepare("SELECT rotation, cursor, resealed, unreadable, completed_at FROM secrets_rotation WHERE id = 1")
        .first<{ rotation: string | null; cursor: string; resealed: number; unreadable: number; completed_at: number | null }>();
      return row
        ? { rotation: row.rotation, cursor: row.cursor, resealed: row.resealed, unreadable: row.unreadable, completedAt: row.completed_at }
        : FRESH;
    },
    async save(s) {
      await db
        .prepare(
          "INSERT INTO secrets_rotation (id, rotation, cursor, resealed, unreadable, completed_at) VALUES (1, ?, ?, ?, ?, ?) " +
            "ON CONFLICT (id) DO UPDATE SET rotation = excluded.rotation, cursor = excluded.cursor, " +
            "resealed = excluded.resealed, unreadable = excluded.unreadable, completed_at = excluded.completed_at",
        )
        .bind(s.rotation, s.cursor, s.resealed, s.unreadable, s.completedAt)
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
