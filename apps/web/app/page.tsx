import Link from "next/link";
import { Code } from "../components/Code";
import { GATEWAY_URL, LIMITS } from "../lib/site";
import {
  agentPrompt,
  hookCall,
  inboxCall,
  mcpClaudeCode,
  mcpRemoteConfig,
  scheduleCall,
  sdkShort,
  taskResult,
  watchActCall,
} from "../lib/snippets";

const CONNECT = [
  [
    "Claude Code",
    <>Run this once, then approve the connection on the page that opens. No account, no API key.</>,
    mcpClaudeCode,
  ],
  [
    "Claude Desktop, Cursor, any MCP client",
    <>
      Add <code>{GATEWAY_URL}/mcp</code> as a remote MCP server (in Claude, a custom connector) and click{" "}
      <strong>Connect</strong> when asked.
    </>,
    mcpRemoteConfig,
  ],
  [
    "Your own agent, in code",
    <>
      Use the <a href="https://agentauthprotocol.com">Agent Auth</a> SDK. The agent makes its own keys and gets its
      own project.
    </>,
    sdkShort,
  ],
] as const;

const CAPABILITIES = [
  ["Do it later", "Send a signed HTTPS request in 90 seconds or 7 days, retried with backoff until it lands.", "schedule_http"],
  ["Do it in order", "Tasks in one lane run one at a time, in order, even through retries.", "lane"],
  ["Do it every day", "Crons on a UTC schedule, with an alert when runs start failing.", "create_cron"],
  ["Wait for a job, then act", "Poll a status URL until it says done, then send the next request itself.", "watch_http · on_match"],
  ["Hear back without a server", "Results, captured responses and alerts land in an inbox the next session reads.", "read_inbox · capture"],
  ["Get woken up", "When the inbox gets something, RunLater calls the trigger that starts your agent.", "set_wake"],
  ["Receive webhooks", "A URL for GitHub, Stripe or CI. Signatures checked, noise filtered, events into the inbox.", "create_hook"],
  ["Keep secrets out of prompts", "Store a token once, bound to its hosts; requests say {{secret:NAME}} instead.", "set_secret"],
] as const;

const EXAMPLES = [
  ["Watch a deploy, then test it", "No agent turn in between: when the deploy reports SUCCEEDED, the smoke tests start.", watchActCall],
  ["Hear only about failed CI", "Register the URL with GitHub. Successful runs are dropped; failures reach the inbox.", hookCall],
  ["Pick up where you left off", "The next session starts from a report, not from checking everything again.", inboxCall],
] as const;

const STEPS = [
  ["Connect", <>Add RunLater to your MCP client, or register from code. The agent gets its own identity.</>],
  ["Create a project", <><code>create_project</code> starts a free trial: 100 delivery attempts over 7 days.</>],
  ["Hand it the waiting", <>Schedule, watch, run a cron or open a hook. Then the session can end.</>],
  ["Hear back", <>Read the inbox next session, get woken up, or check <code>get_task</code>.</>],
] as const;

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
        <div>
          <span className="pill">Open to agents · free trial</span>
          <h1>
            Your agent's session ends. <em>The work doesn't.</em>
          </h1>
          <p className="lede">
            RunLater keeps going after your agent stops: it sends signed HTTPS requests later, watches long jobs and acts
            when they finish, receives webhooks, and keeps every result in an inbox the next session reads. It can even
            wake the agent up.
          </p>
          <div className="actions">
            <a className="button primary" href="#connect">
              Connect your agent
            </a>
            <Link className="button secondary" href="/docs">
              Read the quickstart
            </Link>
          </div>
        </div>
        <div className="demo" aria-label="Example">
          <Code label="the agent, today">{scheduleCall}</Code>
          <Code label="get_task, tomorrow">{taskResult}</Code>
        </div>
      </section>

      <section className="section" id="connect">
        <div className="wrap">
          <p className="eyebrow">Get started</p>
          <h2>Connect your agent in a minute</h2>
          <p className="intro">
            RunLater is a set of tools your agent calls, over MCP or from code. There is no dashboard, no sign-up and no
            API key to paste. Each connection gets its own project with a free trial.
          </p>
          <div className="rows">
            {CONNECT.map(([title, body, code]) => (
              <div className="card" key={title}>
                <div>
                  <h3>{title}</h3>
                  <p>{body}</p>
                </div>
                <Code>{code}</Code>
              </div>
            ))}
          </div>
          <div className="prompt">
            <div>
              <h3>Then just ask</h3>
              <p>
                Your agent reads what each tool does. Describe the outcome and when you want it, and it picks the calls.
              </p>
            </div>
            <Code label="you, to your agent">{agentPrompt}</Code>
          </div>
        </div>
      </section>

      <section className="section">
        <div className="wrap">
          <p className="eyebrow">What your agent can do</p>
          <h2>Everything that has to happen while it's gone</h2>
          <p className="intro">
            Each of these is a tool call. They share the same signed, retried, recorded delivery underneath.
          </p>
          <div className="features four">
            {CAPABILITIES.map(([title, body, tools]) => (
              <div className="card" key={title}>
                <h3>{title}</h3>
                <p>{body}</p>
                <p className="tools">{tools}</p>
              </div>
            ))}
          </div>
        </div>
      </section>

      <section className="section">
        <div className="wrap">
          <p className="eyebrow">Examples</p>
          <h2>What it looks like</h2>
          <p className="intro">
            What the agent sends. In an MCP client you ask in plain words and see these calls go by.
          </p>
          <div className="rows">
            {EXAMPLES.map(([title, body, call]) => (
              <div className="card" key={title}>
                <div>
                  <h3>{title}</h3>
                  <p>{body}</p>
                </div>
                <Code>{call}</Code>
              </div>
            ))}
          </div>
        </div>
      </section>

      <section className="section">
        <div className="wrap">
          <p className="eyebrow">How it works</p>
          <h2>Four steps, no infrastructure</h2>
          <p className="intro">
            No server, queue or cron job to run. RunLater holds the work and the results until your agent comes back.
          </p>
          <ol className="steps">
            {STEPS.map(([title, body]) => (
              <li key={title}>
                <h3>{title}</h3>
                <p>{body}</p>
              </li>
            ))}
          </ol>
        </div>
      </section>

      <section className="section">
        <div className="wrap">
          <p className="eyebrow">Delivery</p>
          <h2>Built to be trusted with “later”</h2>
          <p className="intro">
            Delivery is at least once. A 2xx response means the destination accepted the request; RunLater says so, and
            nothing more.
          </p>
          <div className="features">
            {GUARANTEES.map(([title, body]) => (
              <div className="card" key={title}>
                <h3>{title}</h3>
                <p>{body}</p>
              </div>
            ))}
          </div>
        </div>
      </section>

      <section className="section">
        <div className="wrap limits">
          <div>
            <p className="eyebrow">Limits</p>
            <h2>Free to start, $5 when you need more</h2>
            <p className="muted">
              Everything above is in the trial. When it runs out, your agent sends you a payment link: 50,000 more
              attempts for $5, with no account or subscription. Receiving webhooks and reading the inbox cost nothing.
            </p>
            <Link href="/docs/delivery">How delivery works →</Link>
          </div>
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
        </div>
      </section>

      <section className="wrap cta">
        <h2>Give your agent a sense of later</h2>
        <p>Connect it from an MCP client or from code, and hand it the waiting.</p>
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
