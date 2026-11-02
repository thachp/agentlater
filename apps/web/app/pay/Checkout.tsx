"use client";

// Stripe's Embedded Checkout for the session in the link. The gateway puts the publishable key and
// the session's client secret in the fragment, which never reaches a server.

import { loadStripe, type StripeEmbeddedCheckout } from "@stripe/stripe-js";
import { useEffect, useRef, useState } from "react";

type State = "loading" | "ready" | "paid" | "invalid" | "failed";

export default function Checkout() {
  const mount = useRef<HTMLDivElement>(null);
  const [state, setState] = useState<State>("loading");

  useEffect(() => {
    const fragment = new URLSearchParams(window.location.hash.slice(1));
    const key = fragment.get("key");
    const secret = fragment.get("secret");
    if (!key?.startsWith("pk_") || !secret?.startsWith("cs_")) {
      setState("invalid");
      return;
    }
    let checkout: StripeEmbeddedCheckout | undefined;
    let cancelled = false;
    (async () => {
      const stripe = await loadStripe(key);
      if (!stripe) throw new Error("Stripe.js did not load");
      checkout = await stripe.createEmbeddedCheckoutPage({
        clientSecret: secret,
        onComplete: () => {
          checkout?.destroy();
          setState("paid");
        },
      });
      if (cancelled) return checkout.destroy();
      checkout.mount(mount.current!);
      setState("ready");
    })().catch(() => !cancelled && setState("failed"));
    return () => {
      cancelled = true;
      checkout?.destroy();
    };
  }, []);

  if (state === "paid") {
    return (
      <div className="callout" role="status">
        <p>
          <strong>Payment received.</strong> The attempts are added to the project within a few seconds; your agent can check
          with <code>get_project</code>. A receipt is on its way to your email.
        </p>
      </div>
    );
  }
  if (state === "invalid") {
    return (
      <div className="callout" role="alert">
        <p>This link is incomplete. Open the whole link your agent gave you, or ask it for a new one.</p>
      </div>
    );
  }
  if (state === "failed") {
    return (
      <div className="callout" role="alert">
        <p>The payment form could not load. The link may have expired: ask your agent for a new one.</p>
      </div>
    );
  }
  return (
    <>
      {state === "loading" && <p aria-live="polite">Loading the payment form…</p>}
      <div ref={mount} className="pay-checkout" />
    </>
  );
}
