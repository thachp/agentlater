import type { Metadata } from "next";
import Link from "next/link";
import { Code } from "../../components/Code";
import { GATEWAY_URL } from "../../lib/site";
import {
  mcpChatGpt,
  mcpClaudeCode,
  mcpCodex,
  mcpConfig,
  mcpGrok,
  mcpRemoteConfig,
  sdkExample,
} from "../../lib/snippets";

export const metadata: Metadata = { title: "Quickstart" };

export default function Quickstart() {
  return (
    <>
      <h1>Quickstart</h1>
      <p className="lede">
        Connect an agent, create a project, schedule a request, check it later. Two ways in: from an MCP client, or
        from code with the Agent Auth SDK.
      </p>

      <div className="callout">
        <p>
          <strong>Free trial, then $5 for 50,000 attempts.</strong> Each agent gets one project with 100 delivery attempts
          over 7 days. For more, the agent calls <code>get_payment_link</code> and hands the link to its person, who pays
          by card on runlater.io, or pays itself over <a href="https://mpp.dev">MPP</a> with the <code>mpp_url</code> it
          returns. The attempts stay valid for 12 months and leftovers carry over.
        </p>
      </div>

      <h2 id="identity">How agents sign in</h2>
      <p>
        RunLater uses <a href="https://agentauthprotocol.com">Agent Auth</a>. An agent generates its own keys and
        registers them with the gateway; there is no account, password or API key. Each identity gets one project
        with a free trial. Discovery is at <code>{GATEWAY_URL}/.well-known/agent-configuration</code>.
      </p>
      <p>
        <strong>Keep the keys.</strong> The project belongs to the key that created it. If the keys are lost, so is
        access to the project.
      </p>

      <h2 id="mcp">From an MCP client</h2>
      <p>
        Add RunLater as a remote MCP server at <code>{GATEWAY_URL}/mcp</code>. The first time it connects, your
        client opens a RunLater page in the browser; click <strong>Connect</strong>. There's no account to create:
        that connection gets its own project and free trial, and the client keeps it signed in.
      </p>
      <Code label="Claude Code">{mcpClaudeCode}</Code>
      <Code label="ChatGPT (custom connector, developer mode)">{mcpChatGpt}</Code>
      <Code label="OpenAI Codex">{mcpCodex}</Code>
      <Code label="Grok">{mcpGrok}</Code>
      <Code label="clients that take a JSON config (Cursor, Claude Desktop and others)">{mcpRemoteConfig}</Code>
      <p>
        Menu names move around between releases; what every client needs is the URL above, with OAuth sign-in. Custom
        connectors may depend on your plan, and on a workspace an admin may have to allow them.
      </p>
      <p>
        Each RunLater capability is a tool: <code>create_project</code>, then <code>schedule_http</code>,{" "}
        <code>create_cron</code> or <code>watch_http</code>, and later <code>get_task</code>. If the connection is
        removed, so is access to its project; connecting again starts a new one.
      </p>

      <h3 id="mcp-bridge">Or run the Agent Auth bridge locally</h3>
      <p>
        For an Agent Auth identity instead, add the Agent Auth bridge as a local MCP server. It runs on your machine,
        stores the agent's keys in <code>~/.agent-auth</code> (encrypted with the key you set), and talks to RunLater
        for you.
      </p>
      <Code label="claude_desktop_config.json, or your client's MCP settings">{mcpConfig}</Code>
      <p>
        The bridge offers generic Agent Auth tools rather than one tool per RunLater feature. Ask the agent to schedule
        something with RunLater and it will:
      </p>
      <ol>
        <li>
          call <code>connect_agent</code> with provider <code>{GATEWAY_URL}</code>,
        </li>
        <li>
          use <code>list_capabilities</code> to read what RunLater offers,
        </li>
        <li>
          call <code>execute_capability</code> with <code>create_project</code>, then <code>schedule_http</code>, and
          later <code>get_task</code>.
        </li>
      </ol>

      <h2 id="sdk">From code</h2>
      <p>
        Use <code>@auth/agent</code> in TypeScript or JavaScript. Every RunLater feature is a capability you execute
        by name; results come back in <code>data</code>.
      </p>
      <Code label="agent.mjs">{sdkExample}</Code>

      <h2 id="new-capabilities">When RunLater adds a capability</h2>
      <p>
        An agent is granted the capabilities that existed when it connected. When RunLater adds one, such as the cron
        tools (<code>create_cron</code>, <code>get_cron</code>, <code>list_crons</code>, <code>delete_cron</code>),
        an agent that connected earlier gets <code>capability_not_granted</code> until it asks for it once. The grant
        is immediate; nobody has to approve it.
      </p>
      <ul>
        <li>
          From the MCP bridge, the agent calls <code>request_capability</code> with its <code>agent_id</code> and the
          new capability names. The bridge tells agents to do this on <code>capability_not_granted</code>.
        </li>
        <li>
          From code, call <code>client.requestCapability({"{ agentId, capabilities: [\"create_cron\", \"get_cron\", \"list_crons\", \"delete_cron\"] }"})</code>.
        </li>
      </ul>
      <p>The project, its tasks and its keys stay the same; there is no need to connect again.</p>
      <p>
        The same goes for the inbox tools, <code>read_inbox</code> and <code>ack_inbox</code>. Clients of the hosted{" "}
        <code>/mcp</code> server get new tools without asking.
      </p>

      <h2 id="next">Next</h2>
      <ul>
        <li>
          <Link href="/docs/capabilities">Capabilities</Link>: every argument, result and error.
        </li>
        <li>
          <Link href="/docs/delivery">Delivery &amp; signatures</Link>: what the destination receives, how to verify
          it, and when RunLater retries.
        </li>
      </ul>
    </>
  );
}
