#!/usr/bin/env node
// Moves a paid project to a new owner after its keys or OAuth grant are lost. See docs/DEPLOY.md
// ("Recovering a paid project") for how to check the request first.
//
//   node scripts/recover-project.mjs --from prj_old --to prj_new [--env production]          show what would change
//   node scripts/recover-project.mjs --from prj_old --to prj_new [--env production] --apply  change it
//
// The owner of --to, a project the customer's new connection just created, becomes the owner of
// --from. --to's mapping is removed, and --from's old keys or grant stop working.

import { execFileSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { parseArgs } from "node:util";

const PROJECT_ID = /^prj_[0-9a-f]{24}$/;

/** One statement, so it is atomic: REPLACE removes both rows it conflicts with. Does nothing once applied. */
export function recoverySql(from, to) {
  if (!PROJECT_ID.test(from) || !PROJECT_ID.test(to) || from === to) throw new Error("--from and --to must be two different project ids");
  return (
    `INSERT OR REPLACE INTO runlater_project (host_id, project_id, created_at) ` +
    `SELECT (SELECT host_id FROM runlater_project WHERE project_id = '${to}'), project_id, created_at ` +
    `FROM runlater_project WHERE project_id = '${from}' ` +
    `AND EXISTS (SELECT 1 FROM runlater_project WHERE project_id = '${to}')`
  );
}

function main() {
  const { values } = parseArgs({
    options: {
      from: { type: "string" },
      to: { type: "string" },
      env: { type: "string", default: "production" },
      apply: { type: "boolean", default: false },
    },
  });
  if (!["staging", "production"].includes(values.env)) throw new Error("--env must be staging or production");
  const sql = recoverySql(values.from ?? "", values.to ?? "");

  const gateway = join(dirname(fileURLToPath(import.meta.url)), "../apps/gateway");
  const d1 = (command) => {
    const out = execFileSync("npx", ["wrangler", "d1", "execute", "DB", "--env", values.env, "--remote", "--json", "--command", command], {
      cwd: gateway,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "inherit"],
    });
    return JSON.parse(out)[0];
  };
  const rows = () =>
    d1(`SELECT host_id, project_id, created_at FROM runlater_project WHERE project_id IN ('${values.from}', '${values.to}')`).results;

  const before = rows();
  console.table(before);
  if (before.length !== 2) throw new Error("both projects must exist; nothing changed");
  if (!values.apply) {
    console.log("Dry run: pass --apply to give --from to the owner of --to.");
    return;
  }
  d1(sql);
  console.log("Applied:");
  console.table(rows());
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    main();
  } catch (e) {
    console.error(e.message);
    process.exit(1);
  }
}
