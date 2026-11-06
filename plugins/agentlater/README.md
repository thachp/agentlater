# AgentLater plugin for Claude Code

Connects Claude Code to AgentLater's hosted MCP server and adds a `agentlater` skill that teaches Claude
when and how to use it: scheduling, crons, watches, hooks, secrets, and checking the inbox at the
start of a session.

## Install

```bash
claude plugin marketplace add thachp/agentlater
```

```bash
claude plugin install agentlater@agentlater
```

Or inside Claude Code: `/plugin marketplace add thachp/agentlater`, then `/plugin install agentlater@agentlater`.
The first time Claude calls a AgentLater tool, a consent page opens; click **Connect**. There is no account.

## Moving from the `runlater` plugin

RunLater is now AgentLater. The old plugin keeps working, so you only need to move for new work.

A project belongs to its connection and is never moved. Installing this plugin opens a new connection with a new project and a new trial. Attempts, hooks, secrets and inbox events stay with the old one, so keep `runlater` installed until that work is finished, then remove it:

```bash
claude plugin uninstall runlater@runlater
```

## What's in it

| Path | What |
| --- | --- |
| `.claude-plugin/plugin.json` | Plugin manifest |
| `.claude-plugin/icon.png` | Listing icon, 1024 px, rendered from `apps/web/app/icon.svg` |
| `.mcp.json` | The `agentlater` MCP server, `https://gateway.agentlater.io/mcp` over HTTP |
| `skills/agentlater/SKILL.md` | The skill Claude loads when a task needs AgentLater |
| `skills/agentlater/reference.md` | Limits, error codes, inbox events and the signature scheme |

## Using the skill without the plugin

The skill folder works on its own wherever skills are supported, as long as the AgentLater connector is
added there too. In Claude Desktop or claude.ai, add the connector (Settings → Connectors → Add custom
connector, URL `https://gateway.agentlater.io/mcp`), then zip `skills/agentlater` and upload it under
Settings → Capabilities → Skills.

## Updating

Bump `version` in both `.claude-plugin/plugin.json` and the repository's `.claude-plugin/marketplace.json`
when the skill or server changes, so installed copies pick it up.
