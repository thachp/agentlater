# RunLater plugin for Claude Code

Connects Claude Code to RunLater's hosted MCP server and adds a `runlater` skill that teaches Claude
when and how to use it: scheduling, crons, watches, hooks, secrets, and checking the inbox at the
start of a session.

## Install

```bash
claude plugin marketplace add thachp/runlater
```

```bash
claude plugin install runlater@runlater
```

Or inside Claude Code: `/plugin marketplace add thachp/runlater`, then `/plugin install runlater@runlater`.
The first time Claude calls a RunLater tool, a consent page opens; click **Connect**. There is no account.

## What's in it

| Path | What |
| --- | --- |
| `.claude-plugin/plugin.json` | Plugin manifest |
| `.claude-plugin/icon.png` | Listing icon, 1024 px, rendered from `apps/web/app/icon.svg` |
| `.mcp.json` | The `runlater` MCP server, `https://gateway.runlater.io/mcp` over HTTP |
| `skills/runlater/SKILL.md` | The skill Claude loads when a task needs RunLater |
| `skills/runlater/reference.md` | Limits, error codes, inbox events and the signature scheme |

## Using the skill without the plugin

The skill folder works on its own wherever skills are supported, as long as the RunLater connector is
added there too. In Claude Desktop or claude.ai, add the connector (Settings → Connectors → Add custom
connector, URL `https://gateway.runlater.io/mcp`), then zip `skills/runlater` and upload it under
Settings → Capabilities → Skills.

## Updating

Bump `version` in both `.claude-plugin/plugin.json` and the repository's `.claude-plugin/marketplace.json`
when the skill or server changes, so installed copies pick it up.
