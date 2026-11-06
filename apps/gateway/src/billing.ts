// Selling attempt packs. get_payment_link returns up to two ways to pay for the caller's project:
// - payment_url: a person pays on agentlater.io/pay, Stripe's Embedded Checkout (Stripe.js) for one
//   Checkout Session; Stripe then calls /billing/webhook, which adds the pack.
// - mpp_url: an agent with a payment credential pays itself over MPP (mpp.ts).
// Card details never reach AgentLater. With no Stripe keys configured, billing is off.

import { ATTEMPT_PACK } from "@runlater/contracts/billing";
import { ApiClient, ApiError } from "./api";
import { readLimited } from "./body";
import { MPP_ATTEMPTS_KEY, MPP_PROJECT_KEY, PROJECT_ID, mppEnabled, mppLink } from "./mpp";
import { findProject } from "./projects";

export interface BillingEnv {
  STRIPE_SECRET_KEY?: string;
  /** Public; handed to agentlater.io/pay inside the link. */
  STRIPE_PUBLISHABLE_KEY?: string;
  STRIPE_WEBHOOK_SECRET?: string;
  /** The Stripe profile (`profile_…`) MPP charges are made for; unset turns MPP off. */
  STRIPE_PROFILE_ID?: string;
  /** Stripe's API origin for Checkout; tests point it at a fake. mppx always calls api.stripe.com. */
  STRIPE_API_BASE?: string;
}

const STRIPE_API = "https://api.stripe.com";
const STRIPE_VERSION = "2026-10-28.endive";
const PAY_URL = "https://agentlater.io/pay";
/** Stripe's default: a signed event older than this is a replay. */
const TOLERANCE_SECS = 300;
/** Checkout Session events are a few KiB. */
const MAX_EVENT_BYTES = 256 * 1024;
const PAID_EVENTS = new Set(["checkout.session.completed", "checkout.session.async_payment_succeeded"]);

function checkoutEnabled(env: BillingEnv): boolean {
  return Boolean(env.STRIPE_SECRET_KEY && env.STRIPE_PUBLISHABLE_KEY && env.STRIPE_WEBHOOK_SECRET);
}

export function billingEnabled(env: BillingEnv): boolean {
  return checkoutEnabled(env) || mppEnabled(env);
}

const unavailable = (message: string) => new ApiError(503, { error: { code: "billing_unavailable", message } });

/** The ways to pay for one pack for this project: a Checkout link, an MPP link, or both. */
export async function paymentLink(env: BillingEnv & { PUBLIC_URL: string }, owner: string, projectId: string): Promise<unknown> {
  if (!billingEnabled(env)) throw unavailable("buying attempts is not available yet");
  const link: Record<string, unknown> = {
    attempts: ATTEMPT_PACK.attempts,
    price: { amount_cents: ATTEMPT_PACK.priceCents, currency: ATTEMPT_PACK.currency },
    project_id: projectId,
  };
  if (checkoutEnabled(env)) Object.assign(link, await checkoutLink(env, owner, projectId));
  if (mppEnabled(env)) {
    const mpp = await mppLink(env, projectId);
    Object.assign(link, { mpp_url: mpp.url, mpp_expires_ms: mpp.expires_ms });
  }
  return link;
}

/** Starts a Checkout Session for one pack and returns the page that pays it. */
async function checkoutLink(env: BillingEnv, owner: string, projectId: string) {
  const attempts = ATTEMPT_PACK.attempts.toLocaleString("en-US");
  const form = new URLSearchParams({
    mode: "payment",
    // Cards (and the wallets that carry them) settle at once, so "paid" on the page is true.
    // Filters what the Dashboard enables; this API version no longer takes payment_method_types.
    "allowed_payment_method_types[0]": "card",
    ui_mode: "embedded_page",
    redirect_on_completion: "never",
    "line_items[0][quantity]": "1",
    "line_items[0][price_data][currency]": ATTEMPT_PACK.currency,
    "line_items[0][price_data][unit_amount]": String(ATTEMPT_PACK.priceCents),
    "line_items[0][price_data][product_data][name]": `AgentLater: ${attempts} delivery attempts`,
    "line_items[0][price_data][product_data][description]": `Added to project ${projectId}; valid for 12 months.`,
    "metadata[owner]": owner,
    "metadata[project_id]": projectId,
    "metadata[attempts]": String(ATTEMPT_PACK.attempts),
  });
  const res = await fetch(`${env.STRIPE_API_BASE ?? STRIPE_API}/v1/checkout/sessions`, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${env.STRIPE_SECRET_KEY}`,
      "Content-Type": "application/x-www-form-urlencoded",
      "Stripe-Version": STRIPE_VERSION,
    },
    body: form,
  });
  if (!res.ok) {
    console.error(`stripe: creating a checkout session returned ${res.status}: ${(await res.text()).slice(0, 500)}`);
    throw unavailable("could not start a payment; try again shortly");
  }
  const session = (await res.json()) as { id: string; client_secret: string; expires_at: number };
  // In the fragment, so neither the secret nor the key reaches a server or a Referer.
  const fragment = new URLSearchParams({ key: env.STRIPE_PUBLISHABLE_KEY!, secret: session.client_secret });
  return { payment_url: `${PAY_URL}#${fragment}`, expires_ms: session.expires_at * 1000 };
}

const hex = (bytes: ArrayBuffer) => [...new Uint8Array(bytes)].map((b) => b.toString(16).padStart(2, "0")).join("");

function equal(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}

/** Checks a `Stripe-Signature` header: `t=<unix secs>,v1=<hex HMAC-SHA256 of "t.body">`, possibly several v1. */
export async function verifyStripeSignature(header: string | null, body: string, secret: string, nowSecs: number): Promise<boolean> {
  if (!header) return false;
  const parts = header.split(",").map((p) => p.trim().split("="));
  const t = Number(parts.find(([k]) => k === "t")?.[1]);
  const signatures = parts.filter(([k]) => k === "v1").map(([, v]) => v ?? "");
  if (!Number.isInteger(t) || Math.abs(nowSecs - t) > TOLERANCE_SECS || signatures.length === 0) return false;
  const key = await crypto.subtle.importKey("raw", new TextEncoder().encode(secret), { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  const expected = hex(await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(`${t}.${body}`)));
  return signatures.some((s) => equal(s, expected));
}

interface StripeObject {
  id: string;
  /** Checkout Sessions. */
  payment_status?: string;
  metadata?: Record<string, string>;
}

async function credit(env: { API: Fetcher }, key: CryptoKey, projectId: string, paymentId: string, attempts: number): Promise<Response> {
  try {
    await new ApiClient(env.API, key, projectId).call("POST", "/v1/projects/self/credits", "billing", { payment_id: paymentId, attempts });
  } catch (e) {
    console.error(`stripe: crediting ${paymentId} failed: ${e instanceof ApiError ? JSON.stringify(e.body) : String(e)}`);
    // No such project here (e.g. another environment sharing a test account): retrying cannot help.
    if (e instanceof ApiError && e.body.error.code === "project_not_found") return Response.json({ received: true, ignored: "unknown_project" });
    return Response.json({ error: "credit_failed" }, { status: 500 });
  }
  return Response.json({ received: true });
}

/**
 * Stripe's webhook. A paid Checkout Session adds its pack to the project it was made for; a
 * succeeded MPP PaymentIntent does the same, in case crediting failed when it was charged. The API
 * applies each payment id once, so retries and the overlap are safe; anything but a 2xx makes
 * Stripe retry.
 */
export async function handleStripeWebhook(
  request: Request,
  env: BillingEnv & { DB: D1Database; API: Fetcher },
  key: CryptoKey,
): Promise<Response> {
  if (request.method !== "POST") return new Response(null, { status: 405, headers: { Allow: "POST" } });
  if (!env.STRIPE_WEBHOOK_SECRET) return Response.json({ error: "not_found" }, { status: 404 });
  const tooLarge = () => Response.json({ error: "too_large" }, { status: 413 });
  if (Number(request.headers.get("Content-Length") ?? 0) > MAX_EVENT_BYTES) return tooLarge();
  const bytes = await readLimited(request, MAX_EVENT_BYTES);
  if (bytes === null) return tooLarge();
  const body = new TextDecoder().decode(bytes);
  const now = Math.floor(Date.now() / 1000);
  if (!(await verifyStripeSignature(request.headers.get("Stripe-Signature"), body, env.STRIPE_WEBHOOK_SECRET, now))) {
    return Response.json({ error: "bad_signature" }, { status: 400 });
  }

  let event: { type: string; data: { object: StripeObject } };
  try {
    event = JSON.parse(body);
  } catch {
    return Response.json({ error: "bad_json" }, { status: 400 });
  }
  const object = event.data?.object;
  if (event.type === "payment_intent.succeeded") {
    // Only MPP charges carry these; Checkout's PaymentIntents are credited by their session.
    const projectId = object?.metadata?.[MPP_PROJECT_KEY];
    if (!projectId) return Response.json({ received: true });
    const attempts = Number(object.metadata?.[MPP_ATTEMPTS_KEY]);
    if (!PROJECT_ID.test(projectId) || !Number.isInteger(attempts) || attempts < 1) {
      console.error(`stripe: payment intent ${object.id} has malformed AgentLater metadata`);
      return Response.json({ received: true, ignored: "malformed_metadata" });
    }
    return credit(env, key, projectId, object.id, attempts);
  }
  const session = object;
  // Card payments are paid on completion; delayed methods arrive later as async_payment_succeeded.
  if (!PAID_EVENTS.has(event.type) || session?.payment_status !== "paid") return Response.json({ received: true });

  const { owner, project_id: projectId, attempts } = session.metadata ?? {};
  if (!owner || !projectId || (await findProject(env.DB, owner)) !== projectId) {
    console.error(`stripe: paid session ${session.id} does not match a project`);
    return Response.json({ error: "unknown_project" }, { status: 500 });
  }
  return credit(env, key, projectId, session.id, Number(attempts));
}
