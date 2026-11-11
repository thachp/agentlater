import { ATTEMPT_PACK } from "@runlater/contracts/billing";
import Link from "next/link";
import type { ReactNode } from "react";
import { Code } from "../components/Code";
import { CopyButton } from "../components/CopyButton";
import { Icon } from "../components/Icon";
import { Tabs } from "../components/Tabs";
import { GATEWAY_URL, MAX_ATTEMPTS, TRIAL_ATTEMPTS, TRIAL_DAYS } from "../lib/site";
import { mcpClaudeCode, mcpCodex, mcpRemoteConfig, sdkShort } from "../lib/snippets";

const MCP_URL = `${GATEWAY_URL}/mcp`;
const PACK_ATTEMPTS = ATTEMPT_PACK.attempts.toLocaleString("en-US");
const PACK_PRICE = `$${ATTEMPT_PACK.priceCents / 100}`;

const STEPS = [
  ["Connect once", "Add AgentLater to your MCP client."],
  ["Describe the task", "Tell your agent what to request and when."],
  ["Return to the result", "Read completed tasks from the inbox."],
] as const;

const FEATURES = [
  ["clock", "Schedule it", "Send a signed HTTPS request later, or set up a recurring cron."],
  ["repeat", "Watch for it", "Poll a job’s status until a condition matches or its deadline arrives."],
  ["webhook", "Receive it", "Give external services a webhook URL, with verification and filtering."],
  ["tray", "Pick it up", "Collect outcomes and alerts in an inbox your agent can read next time."],
] as const;

const EXAMPLES = [
  {
    label: "Watch a deploy",
    prompt: "Watch my deployment every 5 minutes. When it succeeds, run the smoke tests and save the result in the inbox.",
    flow: ["Watch the status URL", "Match the completion condition", "Send the follow-on request"],
    note: "A deadline bounds the watch. Your agent does not need to stay connected.",
  },
  {
    label: "Schedule a request",
    prompt: "Send a request to my reminder endpoint tomorrow, then save the delivery result for me.",
    flow: ["Schedule the request", "Attempt delivery at the chosen time", "Record the outcome"],
    note: "Retries handle transient failures. Every attempt is recorded.",
  },
  {
    label: "Receive a webhook",
    prompt: "Create a webhook URL for my CI events. Keep the events I need in the inbox for the next session.",
    flow: ["Create a receiving URL", "Verify and filter incoming events", "Read the inbox next session"],
    note: "Receiving webhooks and reading the inbox do not use delivery attempts.",
  },
];

const AGENT_CARD_URL = `${GATEWAY_URL}/.well-known/agent-card.json`;

const A2A_STEPS: [string, ReactNode][] = [
  [
    "Discover",
    <>
      The Agent Card at{" "}
      <a href={AGENT_CARD_URL}>
        <code>/.well-known/agent-card.json</code>
      </a>{" "}
      describes the delivery skill and where to register.
    </>,
  ],
  ["Authenticate", "Agent Auth lets a compatible runtime register on its own, without an owner clicking Connect."],
  [
    "Delegate & check",
    <>
      Send one delivery request to <code>/a2a</code>, then read its status or cancel it.
    </>,
  ],
];

// Each provider's steps are the ones the quickstart documents.
const PROVIDERS: { label: string; detail: string; app: ReactNode; command?: [string, string]; auth: ReactNode }[] = [
  {
    label: "Claude",
    detail: "Claude app + Claude Code",
    app: "In Claude Desktop or claude.ai, go to Settings → Connectors → Add custom connector and paste this MCP address.",
    command: ["Claude Code · terminal command", mcpClaudeCode],
    auth: "Either way, approve the connection on the page that opens.",
  },
  {
    label: "OpenAI",
    detail: "ChatGPT + Codex",
    app: (
      <>
        On <a href="https://chatgpt.com/plugins">chatgpt.com/plugins</a>, choose + → Add custom MCP server. Paste this
        address and select OAuth.
      </>
    ),
    command: ["Codex · config and sign-in", mcpCodex],
    auth: "ChatGPT availability depends on your plan and workspace permissions. Sign in when it asks.",
  },
  {
    label: "Grok",
    detail: "Grok app",
    app: "On grok.com, open Connectors → Custom and add this MCP address.",
    auth: "Approve the connection on the page that opens. Availability depends on your account.",
  },
  {
    label: "Other clients",
    detail: "Any remote HTTP MCP client",
    app: "Add this address as a remote HTTP MCP server in your client, then sign in when asked.",
    command: ["Clients that take a JSON config", mcpRemoteConfig],
    auth: "Your client must support remote HTTP MCP with OAuth sign-in.",
  },
  {
    label: "Your own code",
    detail: "Build with the Agent Auth SDK",
    app: (
      <>
        Use the <a href="https://agentauthprotocol.com">Agent Auth</a> SDK to connect directly to the gateway. Keep the
        identity in persistent storage.
      </>
    ),
    command: ["JavaScript · SDK example", sdkShort],
    auth: "The agent makes its own keys and gets its own project. The full guide lists every capability.",
  },
];

const FAQ = [
  [
    "Does my agent have to stay online?",
    "No. AgentLater can deliver scheduled requests, watch jobs and receive webhooks after the session ends. Your agent can read the results from its inbox on a later session.",
  ],
  [
    "Is this another agent or model?",
    "No. AgentLater provides MCP tools for reliable HTTP delivery, watches and event handling. Your existing agent decides what to do.",
  ],
  [
    "Can it wake my agent?",
    "A wake endpoint can notify a trigger that starts your agent. It requires a supported external trigger; it does not automatically start every chat client.",
  ],
  [
    "Does a retry guarantee success?",
    "No. Delivery uses bounded retries. Outcomes are recorded, including failures, so your agent can inspect them and decide whether to try again.",
  ],
] as const;

export default function Home() {
  return (
    <>
      <div className="dark">
        <section className="wrap hero">
          <div>
            <p className="eyebrow">MCP tools for work that happens later</p>
            <h1>
              Hand off the <em>waiting.</em>
            </h1>
            <p className="lede">
              Your agent schedules the request. AgentLater handles delivery and keeps the result for its next session.
            </p>
            <div className="actions">
              <a className="button primary" href="#connect">
                Connect your agent
              </a>
              <Link className="button secondary" href="/docs">
                Read the docs
              </Link>
            </div>
            <p className="compat">Works with Claude, ChatGPT and Grok</p>
          </div>

          <figure
            className="story"
            aria-label="Example: an agent schedules a request; AgentLater delivers it and saves the result"
          >
            <figcaption>Example delivery</figcaption>
            <p className="speech">
              <Icon name="chat" size={28} />
              <strong>Run this check in 30 minutes.</strong>
            </p>
            <span className="path" aria-hidden="true" />
            <img className="hub" src="/icon.svg" alt="AgentLater" />
            <span className="path" aria-hidden="true" />
            <p className="disconnect">Your agent can disconnect</p>
            <span className="path" aria-hidden="true" />
            <p className="result">
              <Icon name="check" size={30} />
              <strong>HTTP 200 · Result saved</strong>
            </p>
          </figure>
        </section>
      </div>

      <section className="band quick" aria-label="A concrete AgentLater example">
        <div className="wrap split">
          <div>
            <p className="eyebrow">For example</p>
            <h2>A reminder that outlasts the chat.</h2>
          </div>
          <div>
            <p className="quick-prompt">“In 30 minutes, send a reminder to my HTTPS webhook.”</p>
            <p>
              Your agent schedules it. You close the chat. AgentLater attempts delivery, then saves the outcome for
              your agent to read next session.
            </p>
            <p className="tag">Schedule → deliver → check the result</p>
          </div>
        </div>
      </section>

      <section className="section" id="how">
        <div className="wrap">
          <p className="eyebrow">How it works</p>
          <h2>
            From connected to <em>done.</em>
          </h2>
          <ol className="steps">
            {STEPS.map(([title, body], i) => (
              <li key={title}>
                <b className="number" aria-hidden="true">
                  0{i + 1}
                </b>
                <div>
                  <h3>{title}</h3>
                  <p>{body}</p>
                </div>
              </li>
            ))}
          </ol>
          <div className="trial">
            <strong>Start with {TRIAL_ATTEMPTS} free attempts.</strong>
            <p>
              {TRIAL_DAYS}-day trial. No credit card. {PACK_PRICE} for {PACK_ATTEMPTS} more.
            </p>
            <a className="button primary" href="#connect">
              Get connected <Icon name="arrowRight" />
            </a>
          </div>
        </div>
      </section>

      <section className="section band" id="features">
        <div className="wrap">
          <div className="section-top">
            <div>
              <p className="eyebrow">Built for the time in between</p>
              <h2>
                Less waiting.
                <br />
                More follow-through.
              </h2>
            </div>
            <p className="intro">
              A small set of reliable tools for the work that outlasts a conversation. Your agent chooses the calls;
              you stay focused on the outcome.
            </p>
          </div>
          <div className="features">
            {FEATURES.map(([icon, title, body]) => (
              <article key={title}>
                <Icon name={icon} size={30} />
                <h3>{title}</h3>
                <p>{body}</p>
              </article>
            ))}
          </div>
          <Link className="textlink" href="/docs/capabilities">
            Explore every capability <Icon name="arrowUpRight" size={16} />
          </Link>
        </div>
      </section>

      <section className="section" id="examples">
        <div className="wrap">
          <p className="eyebrow">A little less abstract</p>
          <h2>Just ask your agent.</h2>
          <Tabs
            label="Examples"
            tabs={EXAMPLES.map(({ label, prompt, flow, note }) => ({
              label,
              content: (
                <div className="example">
                  <div>
                    <p className="eyebrow">You, to your agent</p>
                    <blockquote>“{prompt}”</blockquote>
                    <p>{note}</p>
                  </div>
                  <ol>
                    {flow.map((step) => (
                      <li key={step}>
                        <Icon name="check" size={23} />
                        {step}
                      </li>
                    ))}
                  </ol>
                </div>
              ),
            }))}
          />
        </div>
      </section>

      <section className="section band" id="a2a">
        <div className="wrap split a2a">
          <div>
            <p className="eyebrow">Built for agents</p>
            <h2>
              MCP for clients.
              <br />
              <em>A2A for agents.</em>
            </h2>
            <p className="status">A2A 1.0 · JSON-RPC</p>
          </div>
          <div>
            <p className="a2a-intro">
              An agent that speaks the Agent2Agent protocol can hand AgentLater an HTTP request to deliver, now or
              later, and read back what happened. No person signs in.
            </p>
            <ol className="a2a-steps">
              {A2A_STEPS.map(([title, body]) => (
                <li key={title}>
                  <strong>{title}</strong>
                  <span>{body}</span>
                </li>
              ))}
            </ol>
            <p className="fine">
              One message is one HTTP delivery, immediate or scheduled. Streaming, push notifications, crons and
              watches are not part of A2A; they stay with the MCP tools. An A2A client must also implement Agent Auth.
            </p>
            <Link className="textlink" href="/docs/a2a">
              Read the A2A guide <Icon name="arrowUpRight" size={16} />
            </Link>
          </div>
        </div>
      </section>

      <section className="section band-raise" id="connect">
        <div className="wrap">
          <div className="section-top bottom">
            <div>
              <p className="eyebrow">Get started</p>
              <h2>Your agent. One connection.</h2>
            </div>
            <p className="intro">
              Choose your provider. Copy the address into your app, or use the command in your terminal.
            </p>
          </div>
          <Tabs
            rail
            label="Choose your provider"
            tabs={PROVIDERS.map(({ label, detail, app, command, auth }) => ({
              label,
              detail,
              content: (
                <div className="sheet">
                  <div className="sheet-top">
                    <span className="eyebrow">{label}</span>
                    <span className="badge">Remote MCP</span>
                  </div>
                  <p className="field-label">MCP server address</p>
                  <div className="copyfield">
                    <code>{MCP_URL}</code>
                    <CopyButton text={MCP_URL} label="Copy the server address" />
                  </div>
                  <p className="sheet-app">{app}</p>
                  {command ? <Code label={command[0]}>{command[1]}</Code> : null}
                  <p className="fine">{auth}</p>
                  <div className="sheet-next">
                    <Icon name="check" size={22} />
                    <p>
                      <strong>Connected? Just ask.</strong>
                      <br />
                      “Create my AgentLater project.”
                    </p>
                  </div>
                  <Link href="/docs" className="textlink">
                    Full setup guide <Icon name="arrowUpRight" size={16} />
                  </Link>
                </div>
              ),
            }))}
          />
        </div>
      </section>

      <section className="section" id="pricing">
        <div className="wrap">
          <div className="section-top">
            <div>
              <p className="eyebrow">Straightforward pricing</p>
              <h2>
                Free to start.
                <br />
                Pay when you need more.
              </h2>
            </div>
            <p className="intro">
              No subscription. All capabilities are included. Receiving webhooks and reading the inbox cost nothing.
            </p>
          </div>
          <div className="prices">
            <article>
              <p className="eyebrow">Try it out</p>
              <p className="price">
                $0 <span>/ free trial</span>
              </p>
              <h3>{TRIAL_ATTEMPTS} delivery attempts</h3>
              <p>
                Valid for {TRIAL_DAYS} days per project. The trial starts when your agent creates a project.
              </p>
              <a href="#connect" className="button outline">
                Connect your agent <Icon name="arrowRight" />
              </a>
            </article>
            <article>
              <p className="eyebrow">Keep it going</p>
              <p className="price">
                {PACK_PRICE} <span>/ top-up</span>
              </p>
              <h3>{PACK_ATTEMPTS} more attempts</h3>
              <p>Valid for 12 months. Your agent can generate a payment link when you need more.</p>
              <Link href="/docs" className="textlink">
                See the documentation <Icon name="arrowUpRight" size={16} />
              </Link>
            </article>
          </div>
          <div className="split explainer">
            <div>
              <h3>What counts as an attempt?</h3>
              <p>
                One attempt is one try to deliver an HTTP request. Successes and failed tries both count against your
                balance.
              </p>
            </div>
            <div>
              <p>
                <strong>First try succeeds? 1 attempt.</strong>
                <br />
                Two failed tries, then success? 3 attempts.
              </p>
              <p className="fine">
                Temporary failures may be retried with backoff. A task allows 1–{MAX_ATTEMPTS} attempts, with{" "}
                {MAX_ATTEMPTS} by default; not every error is retried. Retries are included in your attempt balance,
                not free extras.
              </p>
              <Link href="/docs/delivery#retries" className="textlink">
                How retries work <Icon name="arrowUpRight" size={16} />
              </Link>
            </div>
          </div>
          <p className="fine">
            A successful HTTP response means the destination accepted the request, not that downstream work is
            complete. Scheduling the request does not guarantee delivery.
          </p>
        </div>
      </section>

      <section className="section band">
        <div className="wrap trust">
          <Icon name="shield" size={42} />
          <div>
            <h2>
              “Did it happen?”
              <br />
              Should have an answer.
            </h2>
            <p>
              Signed deliveries. Retries with backoff. A record of every attempt. AgentLater reports the outcome so
              your agent can decide what comes next.
            </p>
          </div>
          <Link className="textlink" href="/docs/delivery">
            Read the delivery details <Icon name="arrowUpRight" size={16} />
          </Link>
        </div>
      </section>

      <section className="section">
        <div className="wrap faq">
          <h2>A few good questions.</h2>
          {FAQ.map(([question, answer]) => (
            <details key={question}>
              <summary>{question}</summary>
              <p>{answer}</p>
            </details>
          ))}
        </div>
      </section>

      <section className="dark closing">
        <div className="wrap">
          <h2>
            Leave the waiting
            <br />
            with AgentLater.
          </h2>
          <a className="button primary" href="#connect">
            Connect your agent <Icon name="arrowRight" />
          </a>
        </div>
      </section>
    </>
  );
}
