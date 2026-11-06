#!/usr/bin/env node
// One-time: generate AgentLater's deploy secrets and store them in the GitHub environments.
// Nothing secret is printed or written to disk. Requires `gh` logged in with admin on the repo.
//
//   node scripts/setup-deploy-secrets.mjs            set anything missing
//   node scripts/setup-deploy-secrets.mjs --rotate   replace all generated values
//
// Per environment (staging, production):
//   secret   ASSERTION_PRIVATE_KEY  gateway's Ed25519 seed (hex) for API assertions
//   variable GATEWAY_PUBLIC_KEY     its public key (hex); the API verifies with it
//   secret   BETTER_AUTH_SECRET     Better Auth signing secret
//   secret   SECRETS_KEY            the API's master key for project secrets (32 bytes, hex).
//                                  Never rotated, not even with --rotate: every stored
//                                  secret is sealed with it and would become unreadable.
//   secret   CLOUDFLARE_ACCOUNT_ID  if --account-id is given
// Staging only:
//   secret   RECEIVER_TOKEN         reads recorded deliveries from the test receiver
//
// CLOUDFLARE_API_TOKEN is not generated here; create it in the Cloudflare dashboard.

import { execFileSync } from "node:child_process";
import { createPrivateKey, createPublicKey, randomBytes } from "node:crypto";

const REPO = "thachp/runlater";
const ENVS = ["staging", "production"];
const args = process.argv.slice(2);
const rotate = args.includes("--rotate");
const accountFlag = args.indexOf("--account-id");
// Only when the flag is given; otherwise the configured account id is left alone.
const accountId = accountFlag === -1 ? undefined : args[accountFlag + 1];
if (accountFlag !== -1 && !/^[0-9a-f]{32}$/.test(accountId ?? "")) {
  console.error("--account-id must be a 32-character hex Cloudflare account id");
  process.exit(2);
}

const gh = (argv, input) =>
  execFileSync("gh", [...argv, "--repo", REPO], { input, encoding: "utf8", stdio: ["pipe", "pipe", "inherit"] });
const existing = (kind, env) =>
  new Set(
    gh([kind, "list", "--env", env, "--json", "name", "--jq", ".[].name"])
      .split("\n")
      .filter(Boolean),
  );

function keypair() {
  const seed = randomBytes(32);
  const der = Buffer.concat([Buffer.from("302e020100300506032b657004220420", "hex"), seed]);
  const pub = createPublicKey(createPrivateKey({ key: der, format: "der", type: "pkcs8" }))
    .export({ format: "der", type: "spki" })
    .subarray(-32)
    .toString("hex");
  return { seed: seed.toString("hex"), pub };
}

for (const env of ENVS) {
  const secrets = existing("secret", env);
  const vars = existing("variable", env);
  const done = [];

  // The key pair must change together, or the API will reject the gateway.
  if (rotate || !secrets.has("ASSERTION_PRIVATE_KEY") || !vars.has("GATEWAY_PUBLIC_KEY")) {
    const { seed, pub } = keypair();
    gh(["secret", "set", "ASSERTION_PRIVATE_KEY", "--env", env], seed);
    gh(["variable", "set", "GATEWAY_PUBLIC_KEY", "--env", env, "--body", pub]);
    done.push("assertion key pair");
  }
  if (rotate || !secrets.has("BETTER_AUTH_SECRET")) {
    gh(["secret", "set", "BETTER_AUTH_SECRET", "--env", env], randomBytes(32).toString("hex"));
    done.push("BETTER_AUTH_SECRET");
  }
  if (!secrets.has("SECRETS_KEY")) {
    gh(["secret", "set", "SECRETS_KEY", "--env", env], randomBytes(32).toString("hex"));
    done.push("SECRETS_KEY");
  }
  if (env === "staging" && (rotate || !secrets.has("RECEIVER_TOKEN"))) {
    gh(["secret", "set", "RECEIVER_TOKEN", "--env", env], randomBytes(32).toString("hex"));
    done.push("RECEIVER_TOKEN");
  }
  if (accountId && (rotate || !secrets.has("CLOUDFLARE_ACCOUNT_ID"))) {
    gh(["secret", "set", "CLOUDFLARE_ACCOUNT_ID", "--env", env], accountId);
    done.push("CLOUDFLARE_ACCOUNT_ID");
  }
  console.log(`${env}: ${done.length ? `set ${done.join(", ")}` : "nothing to do"}`);
  if (!secrets.has("CLOUDFLARE_API_TOKEN")) console.log(`${env}: still missing CLOUDFLARE_API_TOKEN (create it in Cloudflare)`);
}
