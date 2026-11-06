#!/usr/bin/env node
// Delivery lateness and usage from Workers Analytics Engine. See docs/DEPLOY.md.
//
//   CLOUDFLARE_ACCOUNT_ID=... CLOUDFLARE_STATS_TOKEN=... node scripts/stats.mjs [--days 7] [--env production] [--project prj_...]
//
// The token needs Account Analytics Read. Rows are written by the API's record_stat:
//   index1 project id, blob1 "attempt" or "payment", blob2 "real" or "probe",
//   double1 lateness in ms (-1 when not measured) or attempts bought.

import { parseArgs } from "node:util";

const { values } = parseArgs({
  options: {
    days: { type: "string", default: "7" },
    env: { type: "string", default: "production" },
    project: { type: "string" },
  },
});
const { CLOUDFLARE_ACCOUNT_ID: account, CLOUDFLARE_STATS_TOKEN: token } = process.env;
const days = Number(values.days);
if (!account || !token) {
  console.error("CLOUDFLARE_ACCOUNT_ID and CLOUDFLARE_STATS_TOKEN are required");
  process.exit(1);
}
if (!Number.isInteger(days) || days < 1 || days > 92) {
  console.error("--days must be a whole number from 1 to 92");
  process.exit(1);
}
if (!["staging", "production"].includes(values.env)) {
  console.error("--env must be staging or production");
  process.exit(1);
}
if (values.project !== undefined && !/^prj_[0-9a-f]{24}$/.test(values.project)) {
  console.error("--project must be a project id");
  process.exit(1);
}
const one = values.project ? ` AND index1 = '${values.project}'` : "";
const from = `FROM runlater_stats_${values.env} WHERE timestamp > NOW() - INTERVAL '${days}' DAY${one}`;

async function query(sql) {
  const res = await fetch(`https://api.cloudflare.com/client/v4/accounts/${account}/analytics_engine/sql`, {
    method: "POST",
    headers: { Authorization: `Bearer ${token}` },
    body: `${sql} FORMAT JSON`,
  });
  if (!res.ok) throw new Error(`Analytics Engine answered ${res.status}: ${(await res.text()).slice(0, 500)}`);
  return (await res.json()).data;
}

console.log(`${values.env}, last ${days} days${values.project ? `, ${values.project}` : ""}`);

console.log("\nUsage");
console.table(
  await query(
    `SELECT blob2 AS source, COUNT(DISTINCT index1) AS projects, SUM(_sample_interval) AS attempts ${from} AND blob1 = 'attempt' GROUP BY source ORDER BY source`,
  ),
);

console.log("Payments");
console.table(
  await query(
    `SELECT COUNT(DISTINCT index1) AS projects, SUM(_sample_interval) AS packs, SUM(_sample_interval * double1) AS attempts_bought ${from} AND blob1 = 'payment'`,
  ),
);

console.log("Lateness of first attempts (launch target: real p95 under 60000 ms)");
console.table(
  await query(
    `SELECT blob2 AS source, SUM(_sample_interval) AS measured, ` +
      `quantileExactWeighted(0.5)(double1, _sample_interval) AS p50_ms, ` +
      `quantileExactWeighted(0.95)(double1, _sample_interval) AS p95_ms, ` +
      `MAX(double1) AS max_ms ${from} AND blob1 = 'attempt' AND double1 >= 0 GROUP BY source ORDER BY source`,
  ),
);
