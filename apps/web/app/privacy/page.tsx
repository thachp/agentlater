import type { Metadata } from "next";
import Link from "next/link";
import { CONTACT_EMAIL, LEGAL_UPDATED, OPERATOR } from "../../lib/site";

export const metadata: Metadata = { title: "Privacy Policy" };

export default function PrivacyPage() {
  return (
    <div className="wrap prose legal">
      <h1>Privacy Policy</h1>
      <p className="lede">Last updated {LEGAL_UPDATED}.</p>
      <p>
        This policy explains what the hosted RunLater service (runlater.io and gateway.runlater.io), operated by {OPERATOR},
        collects and why. RunLater has no user accounts and does not sell personal information or use it for advertising.
      </p>

      <h2>What we collect</h2>
      <ul>
        <li>
          <strong>Agent identities.</strong> For Agent Auth agents: their public keys, the name they give their host, their
          status and granted capabilities. For hosted MCP connections: the name and redirect address the MCP client registers,
          and its OAuth grant and tokens.
        </li>
        <li>
          <strong>What your agents schedule.</strong> Each task&rsquo;s destination URL, method, headers and body, its schedule,
          lane and callback URL, and each attempt&rsquo;s time and outcome (HTTP status or error). For crons and watches, the
          same request details, plus, for a watch, up to 16 KiB of the last response from the URL it polls.
        </li>
        <li>
          <strong>Payments.</strong> When someone buys attempts, Stripe collects their card details, email and billing information
          under <a href="https://stripe.com/privacy">Stripe&rsquo;s privacy policy</a>; we do not receive or store card numbers.
          We keep the payment&rsquo;s Stripe id, amount, status, and which project it was for.
        </li>
        <li>
          <strong>Technical data.</strong> IP addresses are used to rate-limit registration, sign-in and payment endpoints, and
          Cloudflare records request logs (time, path, status, IP address) that we use to run and secure the Service.
        </li>
      </ul>

      <h2>How long we keep it</h2>
      <ul>
        <li>A task&rsquo;s body is deleted once the task succeeds or is canceled; a failed task keeps it so it can be retried. Finished tasks and their attempts are deleted 30 days after they finish.</li>
        <li>A watch&rsquo;s request headers are deleted when the watch ends.</li>
        <li>Hosted MCP tokens expire after an hour, grants 30 days after last use, and registered clients after 90 days.</li>
        <li>Project balances and payment records are kept while the project exists and as long as we need them for accounting, tax and legal purposes.</li>
        <li>Request logs are kept by Cloudflare for a few days.</li>
      </ul>

      <h2>How we use it</h2>
      <p>
        Only to provide the Service: to deliver and retry requests, report results to your agents, apply limits and payments,
        prevent abuse, and fix problems. We may disclose information if the law requires it or to protect the Service and
        others from harm.
      </p>

      <h2>Who processes it for us</h2>
      <ul>
        <li><strong>Cloudflare</strong> hosts the Service and stores its data (Workers, Durable Objects, D1, KV, Queues, logs).</li>
        <li><strong>Stripe</strong> processes payments.</li>
      </ul>
      <p>
        Requests your agents schedule are sent to the destinations they choose; what those destinations do with the data is up
        to them.
      </p>

      <h2>Cookies</h2>
      <p>
        The site uses no analytics or advertising cookies. The hosted MCP consent page sets a short-lived cookie to protect the
        approval step. The payment page loads Stripe.js, which may set cookies Stripe uses to prevent fraud.
      </p>

      <h2>Your choices</h2>
      <p>
        Your agent can cancel tasks and delete crons at any time. To ask for a copy of a project&rsquo;s data or for it to be
        deleted, email <a href={`mailto:${CONTACT_EMAIL}`}>{CONTACT_EMAIL}</a> with the project id; because there are no
        accounts, we may ask you to prove control of the project, for example by calling <code>get_project</code> with its agent.
        Depending on where you live, you may have further rights under laws such as the GDPR or CCPA; contact us to use them.
      </p>

      <h2>Security</h2>
      <p>
        Traffic is encrypted with TLS, deliveries are signed, the internal API accepts only short-lived signed assertions from the
        gateway, and each project&rsquo;s data is stored separately. No system is perfectly secure; tell us at{" "}
        <a href={`mailto:${CONTACT_EMAIL}`}>{CONTACT_EMAIL}</a> if you find a problem.
      </p>

      <h2>Children</h2>
      <p>The Service is not directed to children under 16, and we do not knowingly collect their information.</p>

      <h2>Changes</h2>
      <p>We may update this policy; the date at the top shows the latest version.</p>

      <h2>Contact</h2>
      <p>
        {OPERATOR} · <a href={`mailto:${CONTACT_EMAIL}`}>{CONTACT_EMAIL}</a>. See also the{" "}
        <Link href="/terms">Terms of Service</Link>.
      </p>
    </div>
  );
}
