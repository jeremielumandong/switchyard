# `swy` command-line reference

`swy` is Switchyard's command-line tool. It runs the same core as the app on the same
profile store and keychain, so it sees your saved connections, follows the same Production
and read-only rules, and writes to the same query history (tagged `cli`).

Getting `swy` on your `PATH`:

- **Linux:** the installer links `~/.local/bin/swy` ([Install (Linux)](../README.md#install-linux)).
- **macOS:** link it from the app bundle ([Install (macOS)](../README.md#install-macos)).
- **Windows:** the installer's *Add swy CLI to PATH* option ([Install (Windows)](../README.md#install-windows)).

`swy --help` and `swy <command> --help` print the same options as below.

## Commands

### `swy connections`

```
swy connections [--json]
```

Lists saved database connections: name, engine and environment, separated by tabs. It never
prints hosts, users or secrets.

| Option | Meaning |
| --- | --- |
| `--json` | A JSON array of `{ name, engine, environment, agent_access }`. |

### `swy query`

```
swy query <connection> [<sql> | --file <path>] [--format table|csv|json] [--limit <n>] [--yes]
```

Runs SQL on a saved connection and prints the results. The SQL comes from the argument,
from `--file`, or from standard input when neither is given. A script can hold several
statements; each result set is printed in turn.

| Option | Default | Meaning |
| --- | --- | --- |
| `-f`, `--file <path>` | | Read the SQL from this file. Can't be combined with `<sql>`. |
| `--format` | `table` | `table`: aligned columns. `csv`: comma-separated with a header row. `json`: one JSON object per row (JSON Lines). |
| `--limit <n>` | `1000` | Rows shown per result set in `table` format. `csv` and `json` print every row. |
| `--yes` | | Run a destructive statement (DROP, TRUNCATE, DELETE or UPDATE without WHERE) on a Production connection. Without it, nothing runs and `swy` exits with an error. |

Results go to standard output. Server notices and "N rows affected" go to standard error,
so you can pipe results safely:

```sh
swy query shop "select id, email from customers" --format csv > customers.csv
swy query shop --file report.sql --format json | jq .
```

A connection locked read-only refuses writes; `--yes` does not change that.

### `swy explain`

```
swy explain <connection> [<sql> | --file <path>] [--analyze] [--open] [--format text|json] [--yes]
```

Captures a query plan and prints it with its hotspots (full scans, bad estimates, spills,
missing indexes and so on).

| Option | Default | Meaning |
| --- | --- | --- |
| `-f`, `--file <path>` | | Read the statement from this file. |
| `--analyze` | | Run the statement for an actual plan. A writing statement runs inside a transaction that is always rolled back. |
| `--open` | | Also show the plan in the running Switchyard app. If the app isn't running, the plan is in its Plan history when you start it. Needs history to be on for the connection. |
| `--format` | `text` | `text`: an indented operator tree with hotspots. `json`: `{ plan, findings, history_id }` with the normalized plan. |
| `--yes` | | Allow an actual plan of a writing statement on a Production connection. |

### `swy workload`

```
swy workload <connection> [--json]
```

Shows the busiest statements and table and index usage statistics, using whatever the engine
provides (for example `pg_stat_statements` or Query Store). When an extension or permission
is missing, the output says what to install or grant instead of failing.

### `swy mcp`

```
swy mcp
```

Serves Switchyard's MCP tools to a coding agent over standard input and output. You don't
normally run it by hand: the app starts it for its assistant, and you can register it with
your own coding CLI ([Use Switchyard from your coding CLI](agents.md)).

## Exit status and logging

`swy` exits with `0` on success and `1` on any error. The error is printed to standard error
as `swy: <message>`. `swy` logs to standard error only at `error` level; set `RUST_LOG`
(for example `RUST_LOG=debug`) for more.

Without an OS keychain (servers, CI), set `SWITCHYARD_SECRETS=vault` and
`SWITCHYARD_VAULT_PASSWORD`. See [Configuration](configuration.md) for these and other
variables.

## MCP tools

`swy mcp` offers these tools. They are all read-only, and the limits are enforced by the
server whatever the coding CLI is configured to allow.

| Tool | Works on | What it does |
| --- | --- | --- |
| `list_connections` | all | Lists the connections and SSH hosts the agent may use: name, engine, environment. |
| `list_tables` | SQL, MongoDB | Tables and views (MongoDB: collections), optionally in one schema. |
| `describe_table` | SQL, MongoDB | Columns, indexes, constraints and foreign keys of a table or view. |
| `run_query` | SQL, MongoDB | One SELECT/WITH in a read-only transaction that is rolled back, or one MongoDB statement that reads. Returns at most `max_rows` rows (default 200, at most 1,000). Timeout `timeout_seconds` (default 30, at most 120). |
| `explain` | SQL, MongoDB | The estimated plan with hotspots. `analyze` asks for an actual plan, which runs only after you approve that exact statement in the app, only in runs the app started, and never on Production. |
| `workload` | SQL | Busiest statements, scans, index usage and missing indexes. |
| `what_if` | PostgreSQL with HypoPG | Plans a statement as if the given `CREATE INDEX` statements had run. Nothing is created. |
| `redis_command` | Redis | One read-only command in `redis-cli` syntax. Writes and `KEYS` are refused. |
| `run_ssh_command` | SSH hosts | One shell command, only in runs the app started, and only after you approve that exact command in the app. Default timeout 60 s. |

Rules that apply to every tool:

- **Only connections with agent access.** A connection is visible to agents only when
  *Allow coding agents* is on in its connection editor. It is off by default, so Production
  connections stay hidden unless someone turns it on for them.
- **Runs started by the app** carry a session token (`SWITCHYARD_MCP_TOKEN`) that narrows
  the server to that run's connections and expires when the run ends.
- **Connection details are scrubbed.** Results and errors name connections only; hosts,
  ports and users are removed.
- **No tool runs DDL or changes data.** Index and statistics suggestions come back as text.
- **Every call is recorded** in query history, tagged `agent` and `agent:<cli>` (from
  `SWITCHYARD_AGENT`).
