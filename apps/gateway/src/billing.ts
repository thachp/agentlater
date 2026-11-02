// Selling attempt packs. An agent asks for a payment link (get_payment_link) and hands it to a
// person; runlater.io/pay shows Stripe's Embedded Checkout (Stripe.js) for that link's Checkout
// Session, and once the payment succeeds Stripe calls /billing/webhook, which adds the pack to the
// project. Card details never reach RunLater. With no Stripe keys configured, billing is off.

import { ATTEMPT_PACK } from "@runlater/contracts/billing";
import { ApiClient, ApiError } from "./api";
import { findProject } from "./projects";

export interface BillingEnv {
  STRIPE_SECRET_KEY?: string;
  /** Public; handed to runlater.io/pay inside the link. */
  STRIPE_PUBLISHABLE_KEY?: string;
  STRIPE_WEBHOOK_SECRET?: string;
  /** Stripe's API origin; tests point it at a fake. */
  STRIPE_API_BASE?: string;
}

const STRIPE_API = "https://api.stripe.com";
const STRIPE_VERSION = "2026-10-28.endive";
const PAY_URL = "https://runlater.io/pay";
/** Stripe's default: a signed event older than this is a replay. */
const TOLERANCE_SECS = 300;
/** Checkout Session events are a few KiB. */
const MAX_EVENT_BYTES = 256 * 1024;
const PAID_EVENTS = new Set(["checkout.session.completed", "checkout.session.async_payment_succeeded"]);

export function billingEnabled(env: BillingEnv): boolean {
  return Boolean(env.STRIPE_SECRET_KEY && env.STRIPE_PUBLISHABLE_KEY && env.STRIPE_WEBHOOK_SECRET);
}

const unavailable = (message: string) => new ApiError(503, { error: { code: "billing_unavailable", message } });

/** Starts a Checkout Session for one pack and returns the link that pays it. */
export async function paymentLink(env: BillingEnv, owner: string, projectId: string): Promise<unknown> {
  if (!billingEnabled(env)) throw unavailable("buying attempts is not available yet");
  const attempts = ATTEMPT_PACK.attempts.toLocaleString("en-US");
  const form = new URLSearchParams({
    mode: "payment",
    ui_mode: "embedded_page",
    redirect_on_completion: "never",
    "line_items[0][quantity]": "1",
    "line_items[0][price_data][currency]": ATTEMPT_PACK.currency,
    "line_items[0][price_data][unit_amount]": String(ATTEMPT_PACK.priceCents),
    "line_items[0][price_data][product_data][name]": `RunLater: ${attempts} delivery attempts`,
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
  return {
    payment_url: `${PAY_URL}#${fragment}`,
    expires_ms: session.expires_at * 1000,
    attempts: ATTEMPT_PACK.attempts,
    price: { amount_cents: ATTEMPT_PACK.priceCents, currency: ATTEMPT_PACK.currency },
    project_id: projectId,
  };
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

interface CheckoutSession {
  id: string;
  payment_status: string;
  metadata?: Record<string, string>;
}

/**
 * Stripe's webhook. A paid Checkout Session adds its pack to the project it was made for. The API
 * applies each session once, so Stripe's retries are safe; anything but a 2xx makes Stripe retry.
 */
export async function handleStripeWebhook(
  request: Request,
  env: BillingEnv & { DB: D1Database; API: Fetcher },
  key: CryptoKey,
): Promise<Response> {
  if (request.method !== "POST") return new Response(null, { status: 405, headers: { Allow: "POST" } });
  if (!env.STRIPE_WEBHOOK_SECRET) return Response.json({ error: "not_found" }, { status: 404 });
  if (Number(request.headers.get("Content-Length") ?? 0) > MAX_EVENT_BYTES) return Response.json({ error: "too_large" }, { status: 413 });
  const body = await request.text();
  if (body.length > MAX_EVENT_BYTES) return Response.json({ error: "too_large" }, { status: 413 });
  const now = Math.floor(Date.now() / 1000);
  if (!(await verifyStripeSignature(request.headers.get("Stripe-Signature"), body, env.STRIPE_WEBHOOK_SECRET, now))) {
    return Response.json({ error: "bad_signature" }, { status: 400 });
  }

  const event = JSON.parse(body) as { type: string; data: { object: CheckoutSession } };
  const session = event.data?.object;
  // Card payments are paid on completion; delayed methods arrive later as async_payment_succeeded.
  if (!PAID_EVENTS.has(event.type) || session?.payment_status !== "paid") return Response.json({ received: true });

  const { owner, project_id: projectId, attempts } = session.metadata ?? {};
  if (!owner || !projectId || (await findProject(env.DB, owner)) !== projectId) {
    console.error(`stripe: paid session ${session.id} does not match a project`);
    return Response.json({ error: "unknown_project" }, { status: 500 });
  }
  try {
    await new ApiClient(env.API, key, projectId).call("POST", "/v1/projects/self/credits", "billing", {
      payment_id: session.id,
      attempts: Number(attempts),
    });
  } catch (e) {
    console.error(`stripe: crediting session ${session.id} failed: ${e instanceof ApiError ? JSON.stringify(e.body) : String(e)}`);
    return Response.json({ error: "credit_failed" }, { status: 500 });
  }
  return Response.json({ received: true });
}
