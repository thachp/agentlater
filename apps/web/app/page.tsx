import { ATTEMPT_PACK } from "@runlater/contracts/billing";
import Link from "next/link";
import type { ReactNode } from "react";
import { Code } from "../components/Code";
import { CopyButton } from "../components/CopyButton";
import { Tabs } from "../components/Tabs";
import { GATEWAY_URL, LIMITS } from "../lib/site";
import {
  agentPrompt,
  hookCall,
  inboxCall,
  mcpChatGpt,
  mcpClaude,
  mcpClaudeCode,
  mcpCodex,
  mcpGrokCli,
  mcpGrok,
  mcpRemoteConfig,
  readInboxCall,
  scheduleCall,
  sdkShort,
  watchActCall,
} from "../lib/snippets";

const MCP_URL = `${GATEWAY_URL}/mcp`;
const PACK_ATTEMPTS = ATTEMPT_PACK.attempts.toLocaleString("en-US");
const PACK_PRICE = `$${ATTEMPT_PACK.priceCents / 100}`;

// One reminder, followed from the call that schedules it to the session that reads the result.
const TIMELINE: { when: string; title: string; status?: [string, "bad" | "ok"]; note?: string; code?: string }[] = [
  { when: "Today, 4:02 pm", title: "Your agent hands it over", code: scheduleCall },
  { when: "Tomorrow, 4:02 pm", title: "First attempt", status: ["503", "bad"], note: "The destination is down." },
  { when: "Seconds later", title: "Retried, signed", status: ["200", "ok"], note: "The destination accepted it." },
  { when: "Right after", title: "Result recorded", status: ["inbox +1", "ok"], note: "It waits there, for up to 30 days." },
  { when: "Next session", title: "Your agent finds out", code: readInboxCall },
];

const FACTS = [
  ["1,000", "delivery attempts free, per project"],
  [PACK_PRICE, `for ${PACK_ATTEMPTS} more, with no subscription`],
  ["5", "attempts per task, spread over about an hour"],
  ["0", "servers, dashboards or API keys"],
] as const;

const BEFORE_AFTER: [string, ReactNode][] = [
  [
    "It says it will check the deploy in 20 minutes. The session ends. Nobody checks.",
    <>
      <code>watch_http</code> polls until the deploy finishes, then starts the smoke tests itself.
    </>,
  ],
  [
    "It has no public URL, so GitHub and Stripe have nowhere to send anything.",
    <>
      <code>create_hook</code> gives it one. Events wait in the inbox.
    </>,
  ],
  [
    "The next session starts with no idea what the last one left running.",
    <>
      <code>read_inbox</code> opens with a report of what finished.
    </>,
  ],
];

const CONNECT: { label: string; body: ReactNode; code: string }[] = [
  {
    label: "Claude Code",
    body: <>Run the command, then open Claude Code and use <code>/mcp</code> to authenticate AgentLater in your browser.</>,
    code: mcpClaudeCode,
  },
  {
    label: "Codex CLI",
    body: <>Add the server, then run the login command and approve the connection in your browser.</>,
    code: mcpCodex,
  },
  {
    label: "Grok CLI",
    body: <>Add the remote HTTP server. Grok handles OAuth in your browser on first use.</>,
    code: mcpGrokCli,
  },
  {
    label: "Claude app",
    body: <>In Claude Desktop or claude.ai, add a custom connector and approve the connection in your browser.</>,
    code: mcpClaude,
  },
  {
    label: "ChatGPT app",
    body: (
      <>
        On <a href="https://chatgpt.com/plugins">chatgpt.com/plugins</a>, add a custom MCP server with this URL, then
        choose OAuth and approve the connection. Availability depends on your plan and workspace permissions.
      </>
    ),
    code: mcpChatGpt,
  },
  {
    label: "Grok app",
    body: <>Add a custom connector at grok.com and approve the connection on the page that opens.</>,
    code: mcpGrok,
  },
  {
    label: "Other MCP clients",
    body: (
      <>
        Cursor and other MCP clients: add the URL as a remote MCP server and sign in when asked. The{" "}
        <Link href="/docs">quickstart</Link> has the details for each.
      </>
    ),
    code: mcpRemoteConfig,
  },
  {
    label: "Your own code",
    body: (
      <>
        Use the <a href="https://agentauthprotocol.com">Agent Auth</a> SDK. The agent makes its own keys and gets its
        own project.
      </>
    ),
    code: sdkShort,
  },
];

const CAPABILITIES = [
  ["Do it later", "Send a signed HTTPS request in 90 seconds or 7 days, retried with backoff until it lands.", "schedule_http"],
  ["Do it every day", "Crons on a UTC schedule, with an alert when runs start failing.", "create_cron"],
  ["Wait for a job, then act", "Poll a status URL until it says done, then send the next request itself.", "watch_http"],
  ["Receive webhooks", "A URL for GitHub, Stripe or CI. Signatures checked, noise filtered, events into the inbox.", "create_hook"],
  ["Hear back without a server", "Results, captured responses and alerts land in an inbox the next session reads.", "read_inbox"],
  ["Get woken up", "When the inbox gets something, AgentLater calls the trigger that starts your agent.", "set_wake"],
] as const;

const EXAMPLES = [
  ["Watch a deploy", "No agent turn in between: when the deploy reports SUCCEEDED, the smoke tests start.", watchActCall],
  ["Hear only about failed CI", "Register the URL with GitHub. Successful runs are dropped; failures reach the inbox.", hookCall],
  ["Pick up where it left off", "The next session starts from a report, not from checking everything again.", inboxCall],
] as const;

const RETRY_WAITS = ["up to 10 s", "up to 2 min", "up to 20 min", "up to 40 min"];

const GUARANTEES = [
  ["Signed deliveries", "Every request carries an HMAC-SHA256 signature, so the destination knows it came from your project."],
  ["Retries with backoff", "Timeouts, 429s and 5xx responses are retried with growing, jittered delays. Retry-After is respected."],
  ["Every attempt recorded", "Each try keeps its time and HTTP status or error. “Did it happen?” has a real answer."],
  ["Safe to repeat", "With an idempotency key, a retried call returns the same task instead of a duplicate."],
  ["Public HTTPS only", "Destinations must be public https:// names. Private networks, IPs and localhost are refused."],
  ["Secrets stay put", "A secret only goes to the hosts it was set for, and is never shown back, not even in responses."],
] as const;

export default function Home() {
  return (
    <>
      <section className="wrap hero">
        <span className="pill">Works with Claude, ChatGPT and Grok · free trial</span>
        <h1>
          Your agent's session ends. <em>The work doesn't.</em>
        </h1>
        <div className="hero-row">
          <p className="lede">
            AgentLater sends the request, watches the job and receives the webhook after your agent stops. The result
            waits in an inbox for its next session.
          </p>
          <div>
            <div className="actions">
              <a className="button primary" href="#connect">
                Connect your agent
              </a>
              <Link className="button secondary" href="/docs">
                Read the quickstart
              </Link>
            </div>
            <p className="hero-note">No sign-up and no API key to paste.</p>
          </div>
        </div>

        <figure className="timeline">
          <figcaption>
            <span>One reminder, a day apart</span>
            <span>Nothing runs on your side in between</span>
          </figcaption>
          <div className="lanes" aria-hidden="true">
            <div className="lane">
              <span className="seg seg-agent">your agent</span>
              <span className="seg seg-gap">session over</span>
              <span className="seg seg-agent">your agent</span>
            </div>
            <div className="lane">
              <span className="seg seg-later">AgentLater holds it, sends it, retries it, records it</span>
            </div>
          </div>
          <ol className="stops">
            {TIMELINE.map(({ when, title, status, note, code }) => (
              <li key={title}>
                <p className="stop-when">{when}</p>
                <h2 className="stop-title">{title}</h2>
                {status ? <p className={`stop-status ${status[1]}`}>{status[0]}</p> : null}
                {note ? <p className="stop-note">{note}</p> : null}
                {code ? <Code>{code}</Code> : null}
              </li>
            ))}
          </ol>
        </figure>

        <dl className="facts">
          {FACTS.map(([number, label]) => (
            <div key={label}>
              <dt>{number}</dt>
              <dd>{label}</dd>
            </div>
          ))}
        </dl>
      </section>

      <section className="section">
        <div className="wrap">
          <p className="eyebrow">The problem</p>
          <h2 className="statement">
            Agents stop when the session stops. <em>Deploys, retries and webhooks don't.</em>
          </h2>
          <div className="compare">
            <p className="compare-head">Without AgentLater</p>
            <p className="compare-head">With it</p>
            {BEFORE_AFTER.map(([before, after]) => (
              <div className="compare-row" key={before}>
                <p className="before">{before}</p>
                <p className="after">{after}</p>
              </div>
            ))}
          </div>
        </div>
      </section>

      <section className="section" id="connect">
        <div className="wrap">
          <p className="eyebrow">Get started</p>
          <h2>Connect your agent in a minute</h2>
          <p className="intro">
            AgentLater is a set of tools your agent calls over MCP. There is no dashboard, no sign-up and no API key to
            paste. Each connection gets its own project with a free trial.
          </p>
          <div className="url-bar">
            <span className="url-label">Server URL</span>
            <code>{MCP_URL}</code>
            <CopyButton text={MCP_URL} label="Copy the server URL" />
          </div>
          <Tabs
            label="Where your agent runs"
            tabs={CONNECT.map(({ label, body, code }) => ({
              label,
              content: (
                <div className="panel">
                  <p>{body}</p>
                  <Code>{code}</Code>
                </div>
              ),
            }))}
          />
          <div className="prompt">
            <div>
              <h3>Then just ask</h3>
              <p>Your agent reads what each tool does. Describe the outcome and when you want it, and it picks the calls.</p>
            </div>
            <Code label="you, to your agent">{agentPrompt}</Code>
          </div>
        </div>
      </section>

      <section className="section">
        <div className="wrap">
          <p className="eyebrow">What your agent can do</p>
          <h2>Everything that has to happen while it's gone</h2>
          <p className="intro">Each of these is a tool call. They share the same signed, retried, recorded delivery underneath.</p>
          <div className="tool-cards">
            {CAPABILITIES.map(([title, body, tool]) => (
              <Link className="tool-card" href={`/docs/capabilities#${tool}`} key={tool}>
                <h3>{title}</h3>
                <p>{body}</p>
                <p className="tools">{tool} →</p>
              </Link>
            ))}
          </div>
          <p className="all-tools">
            <Link href="/docs/capabilities">See every capability →</Link>
          </p>
        </div>
      </section>

      <section className="section">
        <div className="wrap">
          <p className="eyebrow">Examples</p>
          <h2>What it looks like</h2>
          <p className="intro">What the agent sends. In an MCP client you ask in plain words and see these calls go by.</p>
          <Tabs
            label="Examples"
            tabs={EXAMPLES.map(([label, body, call]) => ({
              label,
              content: (
                <div className="panel">
                  <p>{body}</p>
                  <Code>{call}</Code>
                </div>
              ),
            }))}
          />
        </div>
      </section>

      <section className="section band-dark">
        <div className="wrap">
          <p className="eyebrow">Delivery</p>
          <h2>Built to be trusted with “later”</h2>
          <p className="intro">
            Delivery is at least once. A 2xx response means the destination accepted the request; AgentLater says so,
            and nothing more.
          </p>
          <ol className="retries" aria-label="Five attempts, with the longest wait before each retry">
            <li>
              <span className="dot" />
              <span>Attempt 1</span>
            </li>
            {RETRY_WAITS.map((wait, i) => (
              <li key={wait}>
                <span className="dot" />
                <span>Attempt {i + 2}</span>
                <span className="wait">{wait}</span>
              </li>
            ))}
          </ol>
          <div className="spec">
            {GUARANTEES.map(([title, body]) => (
              <div key={title}>
                <h3>{title}</h3>
                <p>{body}</p>
              </div>
            ))}
          </div>
        </div>
      </section>

      <section className="section">
        <div className="wrap">
          <p className="eyebrow">Pricing</p>
          <h2>Free to start, {PACK_PRICE} when you need more</h2>
          <p className="intro">
            Everything above is in the trial. Receiving webhooks and reading the inbox cost nothing.
          </p>
          <div className="plans">
            <div className="plan">
              <h3>Free trial</h3>
              <p className="price">$0</p>
              <ul>
                <li>1,000 delivery attempts over 7 days, per project</li>
                <li>
                  Starts when your agent calls <code>create_project</code>
                </li>
                <li>No sign-up and no card</li>
              </ul>
            </div>
            <div className="plan plan-paid">
              <h3>More attempts</h3>
              <p className="price">{PACK_PRICE}</p>
              <ul>
                <li>{PACK_ATTEMPTS} delivery attempts, valid 12 months</li>
                <li>Your agent sends you a payment link when the trial runs out</li>
                <li>Leftovers carry over. No account or subscription</li>
              </ul>
            </div>
          </div>
          <details className="limits">
            <summary>Every limit</summary>
            <div className="table-scroll">
              <table>
                <tbody>
                  {LIMITS.map(([k, v]) => (
                    <tr key={k}>
                      <th scope="row">{k}</th>
                      <td>{v}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            <Link href="/docs/delivery">How delivery works →</Link>
          </details>
        </div>
      </section>

      <section className="wrap cta cta-band">
        <h2>Give your agent a sense of later</h2>
        <p>Connect it from Claude, ChatGPT or Grok, and hand it the waiting.</p>
        <div className="actions">
          <a className="button primary" href="#connect">
            Connect your agent
          </a>
          <Link className="button secondary" href="/docs/capabilities">
            See every capability
          </Link>
        </div>
      </section>
    </>
  );
}
