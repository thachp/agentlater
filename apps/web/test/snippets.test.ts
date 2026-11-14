// The docs' code samples are run, not just shown. The vector is the one pinned in
// crates/egress/src/signing.rs (matches_a_fixed_vector).

import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { describe, expect, it } from "vitest";
import { ATTEMPT_PACK } from "@runlater/contracts/billing";
import { A2A_LIMITS, A2A_PAYLOAD, A2A_STATES, LIMITS, RETRIES } from "../lib/site";
import { a2aConnect, a2aRequest, a2aResponse, a2aRun, a2aSend, verifyNode, verifyPython } from "../lib/snippets";

const SIG = "78034213583182231b9638987a3393e0a8a1a8ba1fa5810e5822c53d835a2eb2";
const headers = (signature: string, ts = "1767225600") => ({
  "runlater-task-id": "task_1",
  "runlater-attempt": "1",
  "runlater-timestamp": ts,
  "runlater-signature": signature,
});

describe("Node verification sample", async () => {
  const file = join(mkdtempSync(join(tmpdir(), "runlater-docs-")), "verify.mjs");
  writeFileSync(file, verifyNode);
  const { verifyAgentLater } = (await import(pathToFileURL(file).href)) as {
    verifyAgentLater: (h: Record<string, string>, body: string | Buffer, secret: string, tol?: number) => boolean;
  };

  it("accepts a genuine signature", () => {
    expect(verifyAgentLater(headers(`v1=${SIG}`), "{}", "secret", Infinity)).toBe(true);
    expect(verifyAgentLater(headers(`v1=${SIG}`), Buffer.from("{}"), "secret", Infinity)).toBe(true);
  });

  it("accepts any entry during rotation", () => {
    expect(verifyAgentLater(headers(`v1=${"0".repeat(64)}, v1=${SIG}`), "{}", "secret", Infinity)).toBe(true);
  });

  it("returns false, never throws, on malformed headers", () => {
    const good = headers(`v1=${SIG}`);
    const { "runlater-task-id": _t, ...noTask } = good;
    const { "runlater-attempt": _a, ...noAttempt } = good;
    const { "runlater-signature": _s, ...noSig } = good;
    for (const h of [
      noTask,
      noAttempt,
      noSig,
      headers(`v1=${SIG}`, "abc"),
      headers(`v1=${SIG}`, "²"),
      headers(`v1=${SIG}`, "-1"),
      headers("v1=é" + "0".repeat(63)),
      headers("v1"),
      headers(""),
      {},
    ]) {
      expect(verifyAgentLater(h as Record<string, string>, "{}", "secret", Infinity)).toBe(false);
    }
  });

  it("rejects tampering, wrong secrets, other schemes and stale timestamps", () => {
    expect(verifyAgentLater(headers(`v1=${SIG}`), "{ }", "secret", Infinity)).toBe(false);
    expect(verifyAgentLater(headers(`v1=${SIG}`), "{}", "other", Infinity)).toBe(false);
    expect(verifyAgentLater(headers(`v0=${SIG}`), "{}", "secret", Infinity)).toBe(false);
    expect(verifyAgentLater(headers("v1=abc"), "{}", "secret", Infinity)).toBe(false);
    expect(verifyAgentLater(headers(`v1=${SIG}`), "{}", "secret")).toBe(false);
  });
});

describe("Python verification sample", () => {
  it("agrees with the same vector", () => {
    const script = `${verifyPython}

h = {"runlater-task-id": "task_1", "runlater-attempt": "1", "runlater-timestamp": "1767225600",
     "runlater-signature": "v1=${SIG}"}
inf = float("inf")
assert verify_runlater(h, b"{}", "secret", inf)
assert not verify_runlater(h, b"{ }", "secret", inf)
assert not verify_runlater(h, b"{}", "other", inf)
assert not verify_runlater(h, b"{}", "secret")
h["runlater-signature"] = "v1=" + "0" * 64 + ", v1=${SIG}"
assert verify_runlater(h, b"{}", "secret", inf)
good = dict(h, **{"runlater-signature": "v1=${SIG}"})
malformed = [
    {k: v for k, v in good.items() if k != "runlater-task-id"},
    {k: v for k, v in good.items() if k != "runlater-attempt"},
    {k: v for k, v in good.items() if k != "runlater-signature"},
    dict(good, **{"runlater-timestamp": "abc"}),
    dict(good, **{"runlater-timestamp": "\u00b2"}),
    dict(good, **{"runlater-timestamp": "-1"}),
    dict(good, **{"runlater-signature": "v1=\u00e9" + "0" * 63}),
    dict(good, **{"runlater-signature": "v1"}),
    dict(good, **{"runlater-signature": ""}),
    {},
]
for m in malformed:
    assert verify_runlater(m, b"{}", "secret", inf) is False, m
print("ok")
`;
    expect(execFileSync("python3", ["-I", "-c", script], { encoding: "utf8" }).trim()).toBe("ok");
  });
});

describe("stated limits", () => {
  const limitsRs = readFileSync(
    join(dirname(fileURLToPath(import.meta.url)), "../../../crates/core/src/limits.rs"),
    "utf8",
  );
  const stated = Object.fromEntries(LIMITS);
  const constant = (name: string) => limitsRs.match(new RegExp(`pub const ${name}: \\w+ = ([^;]+);`))?.[1];

  it("match crates/core", () => {
    expect(constant("MAX_BODY_BYTES")).toBe("64 * 1024");
    expect(stated["Request body"]).toBe("64 KiB");
    expect(constant("MAX_HEADERS")).toBe("32");
    expect(constant("MAX_HEADER_BYTES")).toBe("8 * 1024");
    expect(stated["Headers"]).toBe("32 headers, 8 KiB in total");
    expect(constant("TRIAL_ATTEMPTS")).toBe("1_000");
    expect(constant("TRIAL_DURATION_MS")).toBe("7 * DAY_MS");
    expect(stated["Free trial"]).toContain("1,000 delivery attempts over 7 days");
    expect(constant("MAX_ATTEMPTS")).toBe("5");
    expect(constant("DEFAULT_MAX_ATTEMPTS")).toBe("5");
    expect(stated["Attempts per task"]).toBe("1–5 (default 5)");
    expect(constant("ATTEMPT_TIMEOUT_MS")).toBe("10 * SECOND_MS");
    expect(constant("MAX_RETAINED_TASKS")).toBe("1_000");
    expect(constant("MAX_RETAINED_TASKS_PAID")).toBe("20_000");
    expect(stated["Tasks held"]).toContain("1,000 per project (20,000 once paid)");
    expect(constant("CREDIT_VALIDITY_MS")).toBe("365 * DAY_MS");
    expect(stated["More attempts"]).toBe(
      `${ATTEMPT_PACK.attempts.toLocaleString("en-US")} for $${ATTEMPT_PACK.priceCents / 100}, valid 12 months; leftovers carry over`,
    );
    expect(constant("RETENTION_MS")).toBe("30 * DAY_MS");
    expect(limitsRs.replace(/\s+/g, " ")).toContain(
      "pub const RETRY_DELAYS_MS: [i64; 4] = [ 10 * SECOND_MS, 2 * MINUTE_MS, 20 * MINUTE_MS, 40 * MINUTE_MS, ];",
    );
    expect(RETRIES.map(([, wait]) => wait)).toEqual(["5–10 s", "1–2 min", "10–20 min", "20–40 min"]);
    expect(constant("MAX_SCHEDULE_AHEAD_MS")).toBe("7 * DAY_MS");
    expect(constant("CAPTURE_BODY_BYTES")).toBe("16 * 1024");
    expect(constant("CAPTURE_RETENTION_MS")).toBe("7 * DAY_MS");
    expect(stated["Captured responses"]).toBe("First 16 KiB and 4 headers, kept 7 days after the task finishes");
    expect(constant("MAX_INBOX_EVENTS")).toBe("10_000");
    expect(constant("INBOX_ACKED_RETENTION_MS")).toBe("7 * DAY_MS");
    expect(stated["Inbox"]).toBe("10,000 events per project; unread kept 30 days, acked 7 days");
    expect(constant("MIN_WAKE_DEBOUNCE_MS")).toBe("MINUTE_MS");
    expect(constant("MAX_WAKE_DEBOUNCE_MS")).toBe("DAY_MS");
    expect(limitsRs).toContain('pub const DEFAULT_WAKE_DEBOUNCE: &str = "15m";');
    expect(constant("MAX_WAKE_FAILURES")).toBe("3");
    expect(limitsRs).toContain('pub const DEFAULT_ROTATION_OVERLAP: &str = "24h";');
    expect(constant("MAX_ROTATION_OVERLAP_MS")).toBe("7 * DAY_MS");
    expect(stated["Signing secret rotation"]).toBe("Overlap of 0 seconds to 7 days (default 24 hours)");
    expect(constant("MAX_SECRETS")).toBe("50");
    expect(constant("MAX_SECRET_BYTES")).toBe("4 * 1024");
    expect(constant("MAX_ALLOWED_HOSTS")).toBe("20");
    expect(stated["Secrets"]).toBe("50 per project, 4 KiB each, each bound to at most 20 hosts");
    expect(constant("MAX_HOOKS")).toBe("20");
    expect(constant("HOOK_EVENTS_PER_DAY")).toBe("1_000");
    expect(constant("HOOK_EVENTS_PER_DAY_PAID")).toBe("10_000");
    expect(constant("HOOK_UNVERIFIED_EVENTS_PER_DAY")).toBe("100");
    expect(stated["Hooks"]).toContain("20 per project; 60 requests a minute each; 1,000 events a day (10,000 once paid)");
    expect(stated["Wake"]).toBe("At most one per 1 minute to 24 hours (default 15 minutes); off after 3 failures in a row");
  });
});

// The A2A page shows what the gateway's e2e suite runs (apps/gateway/e2e/agent.e2e.mjs): the
// example client and the wire sample. These tests keep what is shown the same as what is run.
describe("A2A samples", () => {
  const gateway = (file: string) => readFileSync(join(dirname(fileURLToPath(import.meta.url)), "../../gateway", file), "utf8");
  const example = gateway("examples/a2a-client.mjs").split("\n").map((line) => line.trim());

  it("are lines of the example client, in its order", () => {
    for (const snippet of [a2aRun, a2aConnect, a2aSend]) {
      let from = 0;
      for (const line of snippet.split("\n").map((l) => l.trim())) {
        if (line === "" || line === "// …") continue;
        // The commands are in the file's opening comment.
        const at = example.findIndex((l, i) => i >= from && (l === line || l.replace(/^\/\/\s+/, "") === line));
        expect(at, line).toBeGreaterThanOrEqual(0);
        from = at + 1;
      }
    }
  });

  it("are the JSON-RPC request the e2e suite sends and the shape of answer it checks", () => {
    const wire = JSON.parse(gateway("examples/a2a-wire.json")) as { request: unknown; response: unknown };
    expect(JSON.parse(a2aRequest.slice(a2aRequest.indexOf("\n\n")))).toEqual(wire.request);
    expect(a2aRequest).toContain("A2A-Version: 1.0");
    expect(JSON.parse(a2aResponse)).toEqual(wire.response);
  });
});

describe("stated A2A facts", () => {
  const src = (file: string) => readFileSync(join(dirname(fileURLToPath(import.meta.url)), "../../gateway/src", file), "utf8");
  const a2a = src("a2a.ts");
  const auth = src("auth.ts");
  const stated = Object.fromEntries(A2A_LIMITS);

  it("match the gateway: states, payload fields and limits", () => {
    const states = a2a.slice(a2a.indexOf("const STATES"), a2a.indexOf("};", a2a.indexOf("const STATES")));
    expect([...states.matchAll(/(\w+): TaskState\.(\w+),/g)].map((m) => [m[1], m[2]])).toEqual(A2A_STATES.map(([status, state]) => [status, state]));
    expect(a2a).toContain(`const PAYLOAD_FIELDS = ${JSON.stringify(A2A_PAYLOAD.map(([field]) => field)).replaceAll(",", ", ")};`);

    expect(a2a).toContain("At most 60 requests a minute are accepted from one IP address");
    expect(stated["Requests to /a2a"]).toContain("60 a minute from one IP address");
    expect(src("projects.ts")).toContain("export const TOOL_CALLS_PER_MINUTE = 300;");
    expect(stated["Capability calls"]).toContain("300 a minute per host");
    expect(auth).toContain("jwtMaxAge: 60,");
    expect(stated["Token"]).toBe("Lives 60 seconds and is accepted once");
    expect(auth).toContain("const WEEK_SECS = 7 * 24 * 3600;");
    expect(auth).toContain("absoluteLifetime: WEEK_SECS,");
    expect(stated["Agent"]).toContain("Revoked 7 days after it is registered");
    // /a2a reads its body under the gateway's one limit, and that limit is what the page states.
    expect(a2a).toMatch(/import \{[^}]*\bMAX_BODY_BYTES\b[^}]*\} from "\.\/body";/);
    expect(a2a).toContain("await readLimited(request, MAX_BODY_BYTES);");
    expect(src("body.ts")).toContain("export const MAX_BODY_BYTES = 192 * 1024;");
    expect(stated["Request size"]).toBe("192 KiB");
    expect(a2a).toContain("pageSize = 50,");
    expect(a2a).toContain("pageSize < 1 || pageSize > 100");
    expect(stated["ListTasks page"]).toBe("1 to 100 tasks (default 50)");
  });
});
