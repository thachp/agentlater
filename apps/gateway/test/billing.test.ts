import { createHmac } from "node:crypto";
import { describe, expect, it } from "vitest";
import { ApiError } from "../src/api";
import { verifyStripeSignature } from "../src/billing";
import { runCapability } from "../src/projects";

const SECRET = "whsec_test_secret";
const BODY = '{"type":"checkout.session.completed"}';
const NOW = 1_800_000_000;
const sign = (t: number, body = BODY, secret = SECRET) => createHmac("sha256", secret).update(`${t}.${body}`).digest("hex");

describe("verifyStripeSignature", () => {
  it("accepts Stripe's signature, including among several", async () => {
    expect(await verifyStripeSignature(`t=${NOW},v1=${sign(NOW)}`, BODY, SECRET, NOW)).toBe(true);
    expect(await verifyStripeSignature(`t=${NOW},v1=${"0".repeat(64)},v1=${sign(NOW)},v0=x`, BODY, SECRET, NOW + 10)).toBe(true);
  });

  it("rejects a changed body, a wrong secret, an old timestamp or a missing header", async () => {
    expect(await verifyStripeSignature(`t=${NOW},v1=${sign(NOW)}`, BODY + " ", SECRET, NOW)).toBe(false);
    expect(await verifyStripeSignature(`t=${NOW},v1=${sign(NOW, BODY, "whsec_other")}`, BODY, SECRET, NOW)).toBe(false);
    expect(await verifyStripeSignature(`t=${NOW},v1=${sign(NOW)}`, BODY, SECRET, NOW + 301)).toBe(false);
    expect(await verifyStripeSignature(`v1=${sign(NOW)}`, BODY, SECRET, NOW)).toBe(false);
    expect(await verifyStripeSignature(`t=${NOW}`, BODY, SECRET, NOW)).toBe(false);
    expect(await verifyStripeSignature(null, BODY, SECRET, NOW)).toBe(false);
  });
});

describe("running out of attempts", () => {
  const db = { prepare: () => ({ bind: () => ({ first: async () => ({ project_id: "prj_1" }) }) }) } as unknown as D1Database;
  const api = {
    fetch: async () => Response.json({ error: { code: "budget_exhausted", message: "not enough delivery attempts left" } }, { status: 402 }),
  } as unknown as Fetcher;
  const stripe = { STRIPE_SECRET_KEY: "sk", STRIPE_PUBLISHABLE_KEY: "pk", STRIPE_WEBHOOK_SECRET: "whsec" };

  it("points the agent at get_payment_link only when billing is on", async () => {
    const key = (await crypto.subtle.generateKey("Ed25519", false, ["sign", "verify"])) as CryptoKeyPair;
    const run = (env: object) =>
      runCapability({ DB: db, API: api, ...env }, key.privateKey, "host", "schedule_http", { url: "https://a.example.com/" }).catch(
        (e: ApiError) => e,
      );
    const on = (await run(stripe)) as ApiError;
    expect(on.status).toBe(402);
    expect(on.body.error).toMatchObject({ code: "budget_exhausted", message: expect.stringContaining("get_payment_link") });
    const off = (await run({})) as ApiError;
    expect(off.body.error.message).not.toContain("get_payment_link");
    await expect(runCapability({ DB: db, API: api }, key.privateKey, "host", "get_payment_link", {})).rejects.toMatchObject({
      body: { error: { code: "billing_unavailable" } },
    });
  });
});
