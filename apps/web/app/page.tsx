import Link from "next/link";
import { Code } from "../components/Code";
import { LIMITS } from "../lib/site";
import { cronCall, delayCall, laneCall, scheduleCall, taskResult } from "../lib/snippets";

const STEPS = [
  ["Connect", <>Register with Agent Auth. The agent gets its own keys — no account, no API key, no human sign-up.</>],
  ["Create a project", <>One call to <code>create_project</code> starts a free trial: 100 delivery attempts over 7 days.</>],
  ["Schedule", <>Call <code>schedule_http</code> with a URL, a body and a time or delay up to 7 days out.</>],
  ["Check", <>Later, <code>get_task</code> shows whether it succeeded and every attempt, or a callback tells you.</>],
] as const;

const MODES = [
  ["Delay a task", "Send it in 90 seconds or 24 hours. An idempotency key makes a repeated call return the same task.", delayCall],
  ["Queue with a lane", "Tasks in one lane run one at a time, in order, even through retries. The result goes to a callback.", laneCall],
  ["Schedule a cron", "Run on a UTC schedule, up to once an hour. When runs start failing, a signed alert says so.", cronCall],
] as const;

const FEATURES = [
  ["Signed deliveries", "Every request carries an HMAC-SHA256 signature, so the destination knows it came from your project."],
  ["Retries with backoff", "Timeouts, 429s and 5xx responses are retried with growing, jittered delays. Retry-After is respected."],
  ["Every attempt recorded", "Each try keeps its time, HTTP status or error. “Did it happen?” has a real answer."],
  ["Safe to repeat", "Pass an idempotency key and a retried schedule_http call returns the same task instead of a duplicate."],
  ["Callbacks when done", "Pass callback_url and the final status and attempts are POSTed there, signed, when the task finishes."],
  ["Alerts when a cron stops", "If a cron's runs fail or can't start, alert_url gets cron.failing, then cron.recovered once it works again."],
  ["Public HTTPS only", "Destinations must be public https:// names. Private networks, IPs and localhost are refused."],
  ["No pile-up after downtime", "If a cron misses several runs, it runs once when it can, not once for every slot it missed."],
  ["Works from MCP", "Add gateway.runlater.io/mcp as a remote MCP server and click Connect. Agents in code use the Agent Auth SDK."],
] as const;

export default function Home() {
  return (
    <>
      <section className="wrap hero">
        <div>
          <span className="pill">Open to agents · free trial</span>
          <h1>
            Make an HTTP request later. <em>Know whether it happened.</em>
          </h1>
          <p className="lede">
            Agents work in short sessions. RunLater holds the request for them: it sends a signed HTTPS call at the
            time they asked for, retries it when the destination fails, and keeps a record they can check afterwards.
          </p>
          <div className="actions">
            <Link className="button primary" href="/docs">
              Read the quickstart
            </Link>
            <Link className="button secondary" href="/docs/capabilities">
              See the capabilities
            </Link>
          </div>
        </div>
        <div className="demo" aria-label="Example">
          <Code label="the agent, today">{scheduleCall}</Code>
          <Code label="get_task, tomorrow">{taskResult}</Code>
        </div>
      </section>

      <section className="section">
        <div className="wrap">
          <p className="eyebrow">Three ways to run later</p>
          <h2>Once, in order, or on a schedule</h2>
          <p className="intro">
            One-off tasks, ordered queues and recurring jobs are the same signed, retried delivery underneath.
          </p>
          <div className="modes">
            {MODES.map(([title, body, call]) => (
              <div className="card" key={title}>
                <h3>{title}</h3>
                <p>{body}</p>
                <Code>{call}</Code>
              </div>
            ))}
          </div>
        </div>
      </section>

      <section className="section">
        <div className="wrap">
          <p className="eyebrow">How it works</p>
          <h2>Four calls, no infrastructure</h2>
          <p className="intro">
            Everything is an Agent Auth capability. There is no dashboard to visit and no server or cron job to run.
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
            {FEATURES.map(([title, body]) => (
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
              One-off tasks, lanes, crons and watches are all included. When the trial runs out, your agent sends you a
              payment link: 50,000 more attempts for $5, no account or subscription.
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
        <p>Start with the quickstart, from an MCP client or in code.</p>
        <Link className="button primary" href="/docs">
          Read the quickstart
        </Link>
      </section>
    </>
  );
}
