// The docs' code samples are run, not just shown. The vector is the one pinned in
// crates/egress/src/signing.rs (matches_a_fixed_vector).

import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { describe, expect, it } from "vitest";
import { ATTEMPT_PACK } from "@runlater/contracts/billing";
import { LIMITS } from "../lib/site";
import { verifyNode, verifyPython } from "../lib/snippets";

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
  const { verifyRunLater } = (await import(pathToFileURL(file).href)) as {
    verifyRunLater: (h: Record<string, string>, body: string | Buffer, secret: string, tol?: number) => boolean;
  };

  it("accepts a genuine signature", () => {
    expect(verifyRunLater(headers(`v1=${SIG}`), "{}", "secret", Infinity)).toBe(true);
    expect(verifyRunLater(headers(`v1=${SIG}`), Buffer.from("{}"), "secret", Infinity)).toBe(true);
  });

  it("accepts any entry during rotation", () => {
    expect(verifyRunLater(headers(`v1=${"0".repeat(64)}, v1=${SIG}`), "{}", "secret", Infinity)).toBe(true);
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
      expect(verifyRunLater(h as Record<string, string>, "{}", "secret", Infinity)).toBe(false);
    }
  });

  it("rejects tampering, wrong secrets, other schemes and stale timestamps", () => {
    expect(verifyRunLater(headers(`v1=${SIG}`), "{ }", "secret", Infinity)).toBe(false);
    expect(verifyRunLater(headers(`v1=${SIG}`), "{}", "other", Infinity)).toBe(false);
    expect(verifyRunLater(headers(`v0=${SIG}`), "{}", "secret", Infinity)).toBe(false);
    expect(verifyRunLater(headers("v1=abc"), "{}", "secret", Infinity)).toBe(false);
    expect(verifyRunLater(headers(`v1=${SIG}`), "{}", "secret")).toBe(false);
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
    expect(constant("TRIAL_ATTEMPTS")).toBe("100");
    expect(constant("TRIAL_DURATION_MS")).toBe("7 * DAY_MS");
    expect(stated["Free trial"]).toContain("100 delivery attempts over 7 days");
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
    expect(stated["Wake"]).toBe("At most one per 1 minute to 24 hours (default 15 minutes); off after 3 failures in a row");
  });
});
