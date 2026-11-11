#!/usr/bin/env node
// Runs locally the checks .github/workflows/ci.yml runs. Change the two together.
//
//   node scripts/check.mjs [--fast]      (or: npm run verify)
//
// --fast skips the two e2e suites. One PASS/FAIL line per step; each step's full output is kept
// in target/check/, and the end of a failing step's output is printed. Stops at the first failure.

import { spawn, spawnSync } from "node:child_process";
import { createWriteStream, existsSync, mkdirSync, readFileSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

const root = fileURLToPath(new URL("..", import.meta.url));
const logs = join(root, "target/check");
const TAIL_LINES = 40;
const { values } = parseArgs({ options: { fast: { type: "boolean", default: false } } });

const step = (name, cwd, cmd, ...args) => ({ name, cwd, cmd, args });
const dryRun = (app) => step(`${app} dry-run build`, `apps/${app}`, "npx", "wrangler", "deploy", "--dry-run", "--outdir", "dist");
const e2e = (app) => step(`${app} e2e`, `apps/${app}`, "npm", "run", "test:e2e");

// In ci.yml's order, except that every dry-run build comes before the e2e suites: the gateway
// suite loads all three builds. An array is a group that runs at once. The e2e suites can, because
// each starts its own workerd on ports the OS hands out, keeps its state in memory and only reads
// the builds.
const steps = [
  step("cargo fmt", ".", "cargo", "fmt", "--all", "--", "--check"),
  step("cargo clippy", ".", "cargo", "clippy", "--locked", "--all-targets", "--", "-D", "warnings"),
  step("cargo test", ".", "cargo", "test", "--locked"),
  step("cargo clippy wasm", ".", "cargo", "clippy", "--locked", "--target", "wasm32-unknown-unknown", "-p", "runlater-api", "--", "-D", "warnings"),
  // CI installs every time; here only a checkout that has no dependencies yet does.
  ...(existsSync(join(root, "node_modules")) ? [] : [step("npm ci", ".", "npm", "ci")]),
  step("typecheck", ".", "npm", "run", "typecheck"),
  step("knip", ".", "npm", "run", "knip"),
  step("unit tests", ".", "npm", "test"),
  dryRun("api"),
  dryRun("gateway"),
  dryRun("receiver"),
  ...(values.fast ? [] : [[e2e("api"), e2e("gateway")]]),
  step("web check", ".", "npm", "run", "check", "-w", "@runlater/web"),
];

/** What `tool --version` prints, or undefined when the tool is not on PATH. */
function version(tool) {
  const { status, stdout } = spawnSync(tool, ["--version"], { cwd: root, encoding: "utf8" });
  return status === 0 ? stdout.trim() : undefined;
}

/** Why the Rust tools on PATH can't run the checks, or undefined when they can. */
function toolchainProblem() {
  const channel = /^channel\s*=\s*"([^"]+)"/m.exec(readFileSync(join(root, "rust-toolchain.toml"), "utf8"))?.[1];
  // Only a pinned version can be compared with what the tools report.
  if (channel && /^\d/.test(channel)) {
    for (const tool of ["cargo", "rustc"]) {
      const found = version(tool);
      if (!found?.startsWith(`${tool} ${channel} `)) {
        return (
          `${tool} on PATH is ${found ? `"${found}"` : "missing"}, but rust-toolchain.toml asks for ${channel}.\n` +
          `Put rustup's ${channel} toolchain first on PATH (\`rustup which cargo\` shows where it is) and run this again.`
        );
      }
    }
  }
  if (!version("worker-build")) {
    return "worker-build is not on PATH; the API's build needs it. The README's Develop section has the install command.";
  }
}

const seconds = (from) => `${((Date.now() - from) / 1000).toFixed(1)}s`;

/** Runs one step with its output in a log file; prints its PASS or FAIL line and says whether it passed. */
async function run({ name, cwd, cmd, args }) {
  const log = join(logs, `${name.replaceAll(" ", "-")}.log`);
  const out = createWriteStream(log);
  const started = Date.now();
  const code = await new Promise((resolve) => {
    const child = spawn(cmd, args, { cwd: join(root, cwd), stdio: ["ignore", "pipe", "pipe"] });
    child.stdout.pipe(out, { end: false });
    child.stderr.pipe(out, { end: false });
    child.on("error", (e) => resolve(`could not start ${cmd}: ${e.message}`));
    child.on("close", resolve);
  });
  await new Promise((done) => out.end(done));
  const passed = code === 0;
  console.log(`${passed ? "PASS" : "FAIL"}  ${name.padEnd(22)} ${seconds(started).padStart(7)}`);
  if (!passed) {
    const lines = readFileSync(log, "utf8").trimEnd().split("\n");
    console.log(`\n--- ${name}: ${typeof code === "number" ? `exit ${code}` : code}; last ${Math.min(TAIL_LINES, lines.length)} lines of ${relative(root, log)}`);
    console.log(lines.slice(-TAIL_LINES).join("\n"));
    console.log("---\n");
  }
  return passed;
}

const problem = toolchainProblem();
if (problem) {
  console.error(`FAIL  toolchain\n${problem}`);
  process.exit(1);
}
mkdirSync(logs, { recursive: true });
const started = Date.now();
for (const [i, group] of steps.entries()) {
  const results = await Promise.all([group].flat().map(run));
  if (results.includes(false)) {
    const left = steps.slice(i + 1).flat().map((s) => s.name);
    console.log(`FAILED after ${seconds(started)}${left.length ? `; not run: ${left.join(", ")}` : ""}`);
    process.exit(1);
  }
}
console.log(`All checks passed in ${seconds(started)}${values.fast ? " (e2e suites skipped)" : ""}`);
