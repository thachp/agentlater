import { ATTEMPT_PACK } from "@runlater/contracts/billing";
import type { Metadata } from "next";
import Link from "next/link";
import { CONTACT_EMAIL, LEGAL_UPDATED, OPERATOR } from "../../lib/site";

export const metadata: Metadata = { title: "Terms of Service" };

const attempts = ATTEMPT_PACK.attempts.toLocaleString("en-US");
const price = `$${(ATTEMPT_PACK.priceCents / 100).toFixed(2)}`;

export default function TermsPage() {
  return (
    <div className="wrap prose legal">
      <h1>Terms of Service</h1>
      <p className="lede">Last updated {LEGAL_UPDATED}.</p>
      <p>
        These terms cover the hosted RunLater service at runlater.io and gateway.runlater.io (the &ldquo;Service&rdquo;),
        operated by {OPERATOR} (&ldquo;RunLater&rdquo;, &ldquo;we&rdquo;). By connecting an agent, scheduling a request or buying
        attempts, you agree to them. &ldquo;You&rdquo; means the person or organization that operates the agent or pays for it;
        you are responsible for what your agents do with the Service. The RunLater source code is separately licensed under
        the MIT License; these terms do not limit that license.
      </p>

      <h2>The Service</h2>
      <p>
        RunLater sends HTTPS requests at times you choose, retries them as described in the <Link href="/docs/delivery">docs</Link>,
        and records each attempt. Delivery is at-least-once: a request can arrive more than once, and you should make your
        endpoints idempotent. We aim to deliver on time but do not guarantee any level of availability or timeliness, and the
        Service may change, be interrupted or be discontinued.
      </p>

      <h2>Projects, trials and attempts</h2>
      <ul>
        <li>Each agent identity (an Agent Auth host, or an approved hosted MCP connection) gets one project. There are no accounts; the keys or OAuth grant that identify your agent are the only way to reach its project. If you lose them, we cannot restore access or move the project to a new identity; see Refunds below.</li>
        <li>A new project includes a free trial of 100 delivery attempts over 7 days. We may change or end the free trial, or limit how many trials are created.</li>
        <li>Each delivery attempt, retry, callback, alert and watch poll uses one attempt. The limits in the <Link href="/docs">docs</Link> apply.</li>
      </ul>

      <h2>Buying attempts</h2>
      <ul>
        <li>An attempt pack is {attempts} delivery attempts for {price} (US dollars, plus any applicable taxes), added to one project. Payments are processed by Stripe; card details never reach us.</li>
        <li>Each purchase makes the project&rsquo;s whole balance valid for 12 months from the purchase. Unused attempts carry over until they expire; expired attempts are not refunded or restored.</li>
        <li>Packs are prepaid and do not renew. There is no subscription to cancel.</li>
        <li>
          <strong>Refunds:</strong> email <a href={`mailto:${CONTACT_EMAIL}`}>{CONTACT_EMAIL}</a> within 14 days of a purchase,
          with the project id or payment receipt, and we will refund it if fewer than 1,000 of that pack&rsquo;s attempts have been
          used. If you lose a project&rsquo;s keys or grant, or they expire, email us from the address on the payment with its receipt
          or details, and we may refund what we estimate is the unused part of its unexpired packs to the original payment method, where
          that method still accepts refunds. A refund does not restore the project, and attempts already in it stay there. Otherwise purchases are final, except where the law requires a refund.
        </li>
        <li>Prices may change; a change does not affect attempts already bought.</li>
      </ul>

      <h2>Acceptable use</h2>
      <p>You may not use the Service to:</p>
      <ul>
        <li>send requests to systems you are not authorized to call, or to attack, overload, scan or probe any system;</li>
        <li>send spam or unsolicited messages, or deliver malware;</li>
        <li>send or store unlawful content, or violate anyone&rsquo;s rights or privacy;</li>
        <li>get around limits or charges, for example by creating identities to collect more free trials;</li>
        <li>interfere with the Service or other users&rsquo; projects.</li>
      </ul>
      <p>
        Do not put secrets in places the docs say are kept or reported, such as URLs. You are responsible for the requests you
        schedule and for having the right to send their contents to their destinations.
      </p>

      <h2>Suspension and termination</h2>
      <p>
        We may limit, suspend or end a project or identity that breaks these terms, puts the Service or others at risk, or as
        required by law, with or without notice. You can stop using the Service at any time. Attempts left in a project that is
        ended for breaking these terms are not refunded.
      </p>

      <h2>Disclaimers</h2>
      <p>
        THE SERVICE IS PROVIDED &ldquo;AS IS&rdquo; AND &ldquo;AS AVAILABLE&rdquo;, WITHOUT WARRANTIES OF ANY KIND, EXPRESS OR
        IMPLIED, INCLUDING MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NON-INFRINGEMENT. WE DO NOT WARRANT THAT ANY
        REQUEST WILL BE DELIVERED, DELIVERED ON TIME, OR DELIVERED ONLY ONCE.
      </p>

      <h2>Limitation of liability</h2>
      <p>
        TO THE EXTENT THE LAW ALLOWS, {OPERATOR.toUpperCase()} WILL NOT BE LIABLE FOR ANY INDIRECT, INCIDENTAL, SPECIAL,
        CONSEQUENTIAL OR PUNITIVE DAMAGES, OR FOR LOST PROFITS, REVENUE OR DATA, ARISING FROM THE SERVICE. OUR TOTAL LIABILITY
        FOR ANY CLAIM RELATING TO THE SERVICE IS LIMITED TO THE GREATER OF THE AMOUNT YOU PAID US IN THE 12 MONTHS BEFORE THE
        CLAIM AND US$50.
      </p>

      <h2>Indemnity</h2>
      <p>
        You will defend and indemnify {OPERATOR} against claims arising from the requests your agents schedule, their contents,
        or your breach of these terms.
      </p>

      <h2>Changes</h2>
      <p>
        We may update these terms. The date at the top shows the latest version; continuing to use the Service after a change
        means you accept it. Material changes to paid attempts apply only to purchases made after the change.
      </p>

      <h2>Governing law</h2>
      <p>
        These terms are governed by the laws of the state in which {OPERATOR} is organized and applicable US federal law,
        without regard to conflict-of-law rules. Courts located in that state have exclusive jurisdiction, except that either
        party may seek injunctive relief in any competent court.
      </p>

      <h2>Contact</h2>
      <p>
        {OPERATOR} · <a href={`mailto:${CONTACT_EMAIL}`}>{CONTACT_EMAIL}</a>. See also the{" "}
        <Link href="/privacy">Privacy Policy</Link>.
      </p>
    </div>
  );
}
