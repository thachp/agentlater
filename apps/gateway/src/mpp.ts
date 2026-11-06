// Buying a pack with MPP (Machine Payments Protocol, https://mpp.dev): an agent that holds a
// payment credential pays for itself, no person or checkout page. get_payment_link returns an
// `mpp_url` bound to the caller's project; POSTing it answers 402 with a Stripe charge challenge,
// and the retry carrying a Shared Payment Token (e.g. from Stripe's link-cli) is charged through a
// PaymentIntent and credited at once. If crediting fails after the charge, Stripe's
// payment_intent.succeeded webhook credits the same PaymentIntent instead (billing.ts).

import { ATTEMPT_PACK } from "@runlater/contracts/billing";
import { Mppx } from "mppx/server/core";
import { charge } from "mppx/stripe/server";
import { ApiClient, ApiError } from "./api";
import type { BillingEnv } from "./billing";

/** PaymentIntent metadata that marks an MPP purchase and where it goes. */
export const MPP_PROJECT_KEY = "runlater_project_id";
export const MPP_ATTEMPTS_KEY = "runlater_attempts";

/** How long an mpp_url stays valid. */
const LINK_TTL_MS = 7 * 24 * 3600 * 1000;
export const PROJECT_ID = /^prj_[0-9a-f]{24}$/;

export function mppEnabled(env: BillingEnv): boolean {
  return Boolean(env.STRIPE_SECRET_KEY && env.STRIPE_PROFILE_ID && env.STRIPE_WEBHOOK_SECRET);
}

const b64url = (bytes: Uint8Array) =>
  btoa(String.fromCharCode(...bytes)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");

/** A key derived from the Stripe secret key for `purpose`, so MPP needs no secret of its own. */
async function derived(secretKey: string, purpose: string): Promise<Uint8Array> {
  const key = await crypto.subtle.importKey("raw", new TextEncoder().encode(secretKey), { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  return new Uint8Array(await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(purpose)));
}

async function linkMac(env: BillingEnv, payload: string): Promise<string> {
  const raw = await derived(env.STRIPE_SECRET_KEY!, "runlater-mpp-link");
  const key = await crypto.subtle.importKey("raw", raw, { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  return b64url(new Uint8Array(await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(payload))));
}

/** `<project id>.<expiry ms>.<mac>`: a link only AgentLater can mint, for one project. */
export async function mppLink(env: BillingEnv & { PUBLIC_URL: string }, projectId: string, nowMs = Date.now()) {
  const payload = `${projectId}.${nowMs + LINK_TTL_MS}`;
  return { url: `${env.PUBLIC_URL}/billing/mpp/${payload}.${await linkMac(env, payload)}`, expires_ms: nowMs + LINK_TTL_MS };
}

/** The project a link pays for, or null if it is forged, malformed or expired. */
export async function readMppLink(env: BillingEnv, token: string, nowMs = Date.now()): Promise<string | null> {
  const [projectId, expires, mac, ...rest] = token.split(".");
  if (rest.length || !projectId || !PROJECT_ID.test(projectId) || !/^\d{1,16}$/.test(expires ?? "") || !mac) return null;
  const expected = await linkMac(env, `${projectId}.${expires}`);
  let diff = expected.length ^ mac.length;
  for (let i = 0; i < Math.min(expected.length, mac.length); i++) diff |= expected.charCodeAt(i) ^ mac.charCodeAt(i);
  if (diff !== 0 || Number(expires) <= nowMs) return null;
  return projectId;
}

/** The PaymentIntent id in a `Payment-Receipt` header (base64url JSON), read without mppx's root barrel. */
function receiptReference(response: Response): string {
  const encoded = response.headers.get("Payment-Receipt") ?? "";
  const json = atob(encoded.replace(/-/g, "+").replace(/_/g, "/"));
  const reference = (JSON.parse(json) as { reference?: unknown }).reference;
  if (typeof reference !== "string" || !reference) throw new Error("no payment reference in the receipt");
  return reference;
}

let cache: { key: string; mppx: Promise<ReturnType<typeof build>> } | undefined;
function build(env: BillingEnv, challengeKey: string) {
  return Mppx.create({
    realm: "agentlater.io",
    secretKey: challengeKey,
    methods: [
      charge({
        secretKey: env.STRIPE_SECRET_KEY!,
        networkId: env.STRIPE_PROFILE_ID!,
        paymentMethodTypes: ["card"],
        currency: ATTEMPT_PACK.currency,
        decimals: 2,
      }),
    ],
  });
}
function mppx(env: BillingEnv) {
  const key = `${env.STRIPE_SECRET_KEY}|${env.STRIPE_PROFILE_ID}`;
  if (cache?.key !== key) {
    cache = { key, mppx: derived(env.STRIPE_SECRET_KEY!, "runlater-mpp-challenge").then((raw) => build(env, b64url(raw))) };
  }
  return cache.mppx;
}

const problem = (status: number, code: string, message: string) =>
  Response.json({ error: { code, message } }, { status, headers: { "Cache-Control": "no-store" } });

/** POST /billing/mpp/<token>: 402 with the charge, or charge the credential and add the pack. */
export async function handleMpp(
  request: Request,
  env: BillingEnv & { API: Fetcher; MPP_RATE: RateLimit },
  key: CryptoKey,
  token: string,
): Promise<Response> {
  if (!mppEnabled(env)) return problem(404, "not_found", "no such route");
  if (request.method !== "POST") return new Response(null, { status: 405, headers: { Allow: "POST" } });
  const projectId = await readMppLink(env, token);
  if (!projectId) return problem(404, "payment_link_invalid", "this payment link is invalid or expired; get_payment_link returns a new one");
  // Each paid attempt is a Stripe API call; keep a leaked link from spending the account's budget.
  const ip = request.headers.get("CF-Connecting-IP") ?? "unknown";
  for (const k of [`ip:${ip}`, `project:${projectId}`]) {
    if (!(await env.MPP_RATE.limit({ key: k })).success) return problem(429, "rate_limited", "too many payment attempts; try again shortly");
  }

  const attempts = ATTEMPT_PACK.attempts;
  const result = await (await mppx(env)).charge({
    amount: (ATTEMPT_PACK.priceCents / 100).toFixed(2),
    description: `AgentLater: ${attempts.toLocaleString("en-US")} delivery attempts for ${projectId}`,
    // Pinned on the credential, so one paid for this project's challenge works only here.
    scope: projectId,
    metadata: { [MPP_PROJECT_KEY]: projectId, [MPP_ATTEMPTS_KEY]: String(attempts) },
  })(request);
  if (result.status === 402) return result.challenge;

  // Charged. Read the PaymentIntent id back from the receipt and credit it.
  let project: unknown;
  let reference = "an unknown payment";
  try {
    reference = receiptReference(result.withReceipt(new Response(null)));
    project = await new ApiClient(env.API, key, projectId).call("POST", "/v1/projects/self/credits", "billing", {
      payment_id: reference,
      attempts,
    });
  } catch (e) {
    // Stripe's payment_intent.succeeded webhook credits it shortly.
    console.error(`mpp: crediting ${reference} for ${projectId} failed: ${e instanceof ApiError ? JSON.stringify(e.body) : String(e)}`);
    return result.withReceipt(
      Response.json({ status: "paid", attempts_pending: attempts, message: "paid; the attempts will be added within a few minutes" }, { status: 202 }),
    );
  }
  return result.withReceipt(Response.json({ status: "paid", attempts_added: attempts, payment_id: reference, project }));
}
