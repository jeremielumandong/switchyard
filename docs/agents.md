# Use Switchyard from your coding CLI

Switchyard's assistant panel (Ctrl/⌘+J) and its **Open in terminal** button already start
Claude Code, Codex CLI, Gemini CLI or a custom CLI with Switchyard's MCP server attached. Use
this guide when you want your own coding CLI session, in your own project, to query and
explain plans on your Switchyard connections.

The server is `swy mcp`. It is read-only, and its limits hold whatever the CLI allows. See
[MCP tools](cli.md#mcp-tools).

## Before you start

1. Make sure `swy` runs in a terminal: `swy --version`. If it doesn't, put it on your `PATH`
   ([`swy` reference](cli.md)).
2. In Switchyard, open each connection the agent may use, turn on **Allow coding agents**,
   and save. Leave it off for anything the agent must not see, especially Production.
3. Check what the agent will see: `swy connections --json` lists every connection with its
   `agent_access` flag.

## Register the server

Pick your CLI. Each example registers a server named `switchyard` that runs `swy mcp`, and
sets `SWITCHYARD_AGENT` so Switchyard's history tags the calls with the right CLI.

### Claude Code

```sh
claude mcp add --env SWITCHYARD_AGENT=claude-code --transport stdio --scope user \
  switchyard -- swy mcp
```

`--scope user` makes it available in all your projects. Leave it out to add it to the
current project only.

### Codex CLI

```sh
codex mcp add switchyard --env SWITCHYARD_AGENT=codex -- swy mcp
```

Or add it to `~/.codex/config.toml` yourself:

```toml
[mcp_servers.switchyard]
command = "swy"
args = ["mcp"]

[mcp_servers.switchyard.env]
SWITCHYARD_AGENT = "codex"
```

### Gemini CLI

Add the server to `~/.gemini/settings.json` (all projects) or `.gemini/settings.json` in a
project:

```json
{
  "mcpServers": {
    "switchyard": {
      "command": "swy",
      "args": ["mcp"],
      "env": { "SWITCHYARD_AGENT": "gemini" }
    }
  }
}
```

`gemini mcp add -e SWITCHYARD_AGENT=gemini switchyard swy mcp` writes the same entry to the
project settings.

### Another CLI

Configure a stdio MCP server with command `swy`, argument `mcp`, and optionally the
environment variable `SWITCHYARD_AGENT` (anything other than `claude-code`, `codex` or
`gemini` is recorded as `custom`).

If your CLI can't find `swy`, use its full path, for example
`/Applications/Switchyard.app/Contents/MacOS/swy` on macOS.

## Check that it works

1. Start the CLI and ask it to list your Switchyard connections. It should call
   `list_connections` and show only the connections you enabled.
2. Ask a question that needs data, for example "how many orders were placed yesterday in
   `shop`?".
3. In Switchyard, open Query history (Ctrl/⌘+Shift+H). The agent's queries are there,
   tagged `agent` and `agent:<cli>`.

## What works only from the app

A session registered this way has no run token from the app, so:

- **SSH hosts** aren't offered, and `run_ssh_command` is refused. Each shell command needs
  your approval in the app, which only works in runs the app started.
- **Actual plans** (`explain` with `analyze`) are refused. Estimated plans work.

Everything else (`list_tables`, `describe_table`, `run_query`, `explain`, `workload`,
`what_if` and `redis_command`) works with the same limits as in the app. To use shell
commands or actual plans, start the CLI from Switchyard's assistant panel or **Open in
terminal**.

Switchyard never reads or stores your CLI's credentials. The CLI uses its own login.
