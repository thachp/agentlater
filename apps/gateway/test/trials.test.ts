import { describe, expect, it } from "vitest";
import { ensureProject } from "../src/projects";

const NOW = 1_800_000_000_000;
const DAY = 24 * 3600 * 1000;

/** The two statements `ensureProject` runs against `runlater_project`, over an in-memory table. */
function fakeDb(rows: { host_id: string; project_id: string; created_at: number }[]) {
  return {
    prepare: (sql: string) => ({
      bind: (...args: unknown[]) => ({
        first: async () => {
          if (sql.includes("COUNT(*)")) return { started: rows.filter((r) => r.created_at > (args[0] as number)).length };
          return rows.find((r) => r.host_id === args[0]) ?? null;
        },
        run: async () => {
          const [host_id, project_id, created_at] = args as [string, string, number];
          if (!rows.some((r) => r.host_id === host_id)) rows.push({ host_id, project_id, created_at });
        },
      }),
    }),
  } as unknown as D1Database;
}
const started = (n: number, at: number) =>
  Array.from({ length: n }, (_, i) => ({ host_id: `h${i}`, project_id: `prj_${i}`, created_at: at }));

describe("new trial cap", () => {
  it("allocates a project under the cap", async () => {
    expect(await ensureProject(fakeDb(started(1, NOW - 1)), "new", 2, NOW)).toMatch(/^prj_/);
  });

  it("refuses a new owner at the cap, saying what to do", async () => {
    await expect(ensureProject(fakeDb(started(2, NOW - 1)), "new", 2, NOW)).rejects.toMatchObject({
      status: 429,
      body: { error: { code: "trials_paused" } },
    });
  });

  it("never refuses an owner who already has a project", async () => {
    expect(await ensureProject(fakeDb(started(2, NOW - 1)), "h0", 2, NOW)).toBe("prj_0");
  });

  it("counts only the last 24 hours", async () => {
    expect(await ensureProject(fakeDb(started(2, NOW - DAY)), "new", 2, NOW)).toMatch(/^prj_/);
  });

  it("has no cap when none is set", async () => {
    expect(await ensureProject(fakeDb(started(5, NOW - 1)), "new", undefined, NOW)).toMatch(/^prj_/);
  });
});
