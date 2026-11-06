import { ATTEMPT_PACK } from "@runlater/contracts/billing";
import type { Metadata } from "next";
import Link from "next/link";
import Checkout from "./Checkout";

export const metadata: Metadata = { title: "Buy delivery attempts", robots: { index: false } };

const attempts = ATTEMPT_PACK.attempts.toLocaleString("en-US");
const price = `$${(ATTEMPT_PACK.priceCents / 100).toFixed(2)}`;

export default function PayPage() {
  return (
    <div className="wrap prose pay">
      <h1>Buy delivery attempts</h1>
      <p className="lede">
        {attempts} delivery attempts for {price}, added to the AgentLater project of the agent that sent you this link. Attempts
        left over carry over, and the whole balance stays valid for 12 months from today.
      </p>
      <Checkout />
      <p className="pay-note">
        Payment is handled by Stripe; AgentLater never sees your card. Links work for 24 hours: if this one has expired, ask your
        agent for a new one with <code>get_payment_link</code>. Buying attempts is covered by the{" "}
        <Link href="/terms">Terms of Service</Link>, including refunds, and the <Link href="/privacy">Privacy Policy</Link>.
      </p>
    </div>
  );
}
