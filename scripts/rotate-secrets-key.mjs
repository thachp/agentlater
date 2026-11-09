#!/usr/bin/env node
// Rotates an environment's SECRETS_KEY (docs/DEPLOY.md, "Rotating SECRETS_KEY"; the design is
// docs/adr/0001-rotating-secrets-key.md). GitHub secrets can't be read back, so the key lives in two
// slots, SECRETS_KEY and SECRETS_KEY_2, and the variable SECRETS_KEY_SLOT says which one is
// current. While SECRETS_KEY_ROTATING is true, the deploy also hands the API the other slot as
// SECRETS_KEY_PREVIOUS. This script only writes and deletes; nothing secret is printed.
//
//   node scripts/rotate-secrets-key.mjs --env production start    new key in the other slot, rotating
//   node scripts/rotate-secrets-key.mjs --env production undo     back to the old key, still rotating
//   node scripts/rotate-secrets-key.mjs --env production finish   rotation over, old key deleted
//
// Deploy after each. Requires `gh` logged in with admin on the repo.

import { execFileSync } from "node:child_process";
import { randomBytes } from "node:crypto";

const REPO = "thachp/agentlater";
const SLOTS = { 1: "SECRETS_KEY", 2: "SECRETS_KEY_2" };
const args = process.argv.slice(2);
const env = args[args.indexOf("--env") + 1];
const step = args.find((a) => ["start", "undo", "finish"].includes(a));
if (!["staging", "production"].includes(env ?? "") || !step || !args.includes("--env")) {
  console.error("usage: node scripts/rotate-secrets-key.mjs --env staging|production start|undo|finish");
  process.exit(2);
}

const gh = (argv, input) =>
  execFileSync("gh", [...argv, "--repo", REPO], { input, encoding: "utf8", stdio: ["pipe", "pipe", "inherit"] });
const variables = Object.fromEntries(
  JSON.parse(gh(["variable", "list", "--env", env, "--json", "name,value"])).map((v) => [v.name, v.value]),
);
const secrets = new Set(JSON.parse(gh(["secret", "list", "--env", env, "--json", "name"])).map((s) => s.name));
const current = variables.SECRETS_KEY_SLOT === "2" ? 2 : 1;
const other = current === 1 ? 2 : 1;
const rotating = variables.SECRETS_KEY_ROTATING === "true";
const fail = (why) => {
  console.error(`${env}: ${why}`);
  process.exit(1);
};

if (!secrets.has(SLOTS[current])) fail(`${SLOTS[current]} (the current key) is not set; nothing to rotate`);
if (step === "start") {
  if (rotating) fail("a rotation is already on; finish or undo it first");
  gh(["secret", "set", SLOTS[other], "--env", env], randomBytes(32).toString("hex"));
  gh(["variable", "set", "SECRETS_KEY_SLOT", "--env", env, "--body", String(other)]);
  gh(["variable", "set", "SECRETS_KEY_ROTATING", "--env", env, "--body", "true"]);
  console.log(`${env}: new key in ${SLOTS[other]}, now current; ${SLOTS[current]} opens until finish. Deploy now.`);
} else if (step === "undo") {
  if (!rotating || !secrets.has(SLOTS[other])) fail("no rotation to undo");
  gh(["variable", "set", "SECRETS_KEY_SLOT", "--env", env, "--body", String(other)]);
  console.log(`${env}: ${SLOTS[other]} is current again; ${SLOTS[current]} still opens. Deploy now, then finish to drop it.`);
} else {
  if (!rotating) fail("no rotation to finish");
  gh(["variable", "set", "SECRETS_KEY_ROTATING", "--env", env, "--body", "false"]);
  if (secrets.has(SLOTS[other])) gh(["secret", "delete", SLOTS[other], "--env", env]);
  console.log(`${env}: rotation over, ${SLOTS[other]} deleted. Deploy now; the deploy removes the API's previous key.`);
}
