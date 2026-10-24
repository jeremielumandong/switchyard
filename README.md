# Switchyard

A fast, native, cross-platform desktop app that puts database querying, SSH terminals,
tunnels and file transfer behind one connection model. It also visualizes query plans,
finds optimization hotspots, and lets coding CLIs (Claude Code, Codex CLI, Gemini CLI or a
custom one) plan and optimize queries through a read-only MCP server.

Around that core sit the tools you would otherwise open next to it: server workspaces with
RDP / VNC launch, commands and a health dashboard across many Hosts, Docker and Kubernetes,
Terraform state, cloud inventory, network checks and logs, runbooks, schema compare, cloud
storage and config services, and an API workbench for HTTP, WebSocket, SSE and gRPC.

Written in Rust on [GPUI](https://www.gpui.rs/) and
[gpui-component](https://github.com/longbridge/gpui-kit). Runs on macOS, Windows and Linux.
Licensed under Apache-2.0.

![Switchyard: a PostgreSQL query and its results in the SQL editor](docs/screenshots/query.png)

<table>
  <tr>
    <td width="50%"><img src="docs/screenshots/plan.png" alt="Query plan with hotspots, flame view and node details"><br><sub><b>Query plans</b>: hotspots, flame view and per-node details from <code>EXPLAIN ANALYZE</code>.</sub></td>
    <td width="50%"><img src="docs/screenshots/table-data.png" alt="Table data in the results grid with WHERE filter and paging"><br><sub><b>Results grid</b>: typed columns, WHERE filter, paging and inline edits.</sub></td>
  </tr>
  <tr>
    <td><img src="docs/screenshots/schema.png" alt="Schema explorer with a table's context menu"><br><sub><b>Schema explorer</b>: lazy tree, row counts and actions on every object.</sub></td>
    <td><img src="docs/screenshots/terminal.png" alt="Local shell terminal tab"><br><sub><b>Terminals</b>: local shells and SSH sessions next to your queries.</sub></td>
  </tr>
  <tr>
    <td><img src="docs/screenshots/new-connection.png" alt="New connection dialog: server, database, file server, cloud service"><br><sub><b>One connection model</b>: servers, databases, file servers and cloud services.</sub></td>
    <td><img src="docs/screenshots/palette.png" alt="Open anything palette filtering connections, tools and commands"><br><sub><b>Open anything</b> (Ctrl/⌘+P): connections, tools and commands in one list.</sub></td>
  </tr>
  <tr>
    <td><img src="docs/screenshots/api-request.png" alt="API explorer: a GET request with query parameters, a pretty JSON response and passing tests"><br><sub><b>API explorer</b>: collections, environments, scripts and tests, next to your databases.</sub></td>
    <td><img src="docs/screenshots/api-runner.png" alt="API explorer collection runner with per-request results"><br><sub><b>Collection runs</b> with iteration data and AI-generated sample data.</sub></td>
  </tr>
  <tr>
    <td><img src="docs/screenshots/api-load.png" alt="Load test of a request with latency percentiles, throughput, a latency histogram and requests per second"><br><sub><b>Load tests</b>: p50 / p95 / p99, throughput and errors, live, with a pass / fail budget.</sub></td>
    <td><img src="docs/screenshots/api-mocks.png" alt="Mock server serving a collection's examples with chaos settings and a call log"><br><sub><b>Mock servers</b> from saved examples, with chaos and record mode.</sub></td>
  </tr>
  <tr>
    <td><img src="docs/screenshots/servers.png" alt="Servers view grouped by folder with the workspace menu open"><br><sub><b>Server workspaces</b>: per-team views, a Servers list with OS and reachability, RDP / VNC.</sub></td>
    <td><img src="docs/screenshots/fleet.png" alt="Fleet run of one command on four Hosts with one Host's output expanded"><br><sub><b>Fleet</b>: one command on many Hosts, with the outlier easy to spot.</sub></td>
  </tr>
  <tr>
    <td><img src="docs/screenshots/containers.png" alt="Docker containers with streamed logs of the selected container"><br><sub><b>Containers</b>: Docker and Kubernetes, logs, shells and port forwards.</sub></td>
    <td><img src="docs/screenshots/terraform.png" alt="Terraform state viewer with a database instance and masked sensitive attributes"><br><sub><b>Terraform state</b>: resources, attributes and outputs, secrets masked.</sub></td>
  </tr>
  <tr>
    <td><img src="docs/screenshots/schema-compare.png" alt="Schema compare between staging and production with a side-by-side DDL diff"><br><sub><b>Schema compare</b>: per-object changes and a migration script to review.</sub></td>
    <td><img src="docs/screenshots/runbook.png" alt="Runbook run with a SQL step's result grid and an HTTP step's response"><br><sub><b>Runbooks</b>: SQL, HTTP, shell and confirm steps with parameters.</sub></td>
  </tr>
</table>

## Quick install (Linux x86_64)

```sh
curl -fsSL https://raw.githubusercontent.com/jeremielumandong/switchyard/main/install.sh | sh
```

Downloads the latest published release, verifies its SHA-256 checksum, and installs
Switchyard with a desktop launcher and the `swy` CLI. No Rust compiler or sudo is needed.
See [Install (Linux)](#install-linux) for requirements, updates and uninstalling.

## Download (macOS)

[Download the latest Switchyard release for macOS](https://github.com/jeremielumandong/switchyard/releases/latest/download/Switchyard-macos-universal.dmg)

A universal app for Apple Silicon and Intel Macs (macOS 11 or later). Open the disk image
and drag Switchyard to Applications.

## Download (Windows x64)

[Download the latest Switchyard release for Windows](https://github.com/jeremielumandong/switchyard/releases/latest/download/Switchyard-windows-x64-setup.exe)

Run the downloaded setup program. It installs for your user only, so it needs no
administrator rights. See [Install (Windows)](#install-windows) for the `swy` CLI, updates
and uninstalling.

## Download (macOS)

[Download the latest Switchyard release for macOS](https://github.com/jeremielumandong/switchyard/releases/latest/download/Switchyard-macos-universal.dmg)

Open the disk image and drag **Switchyard** to **Applications**. One universal build runs
on Apple Silicon and Intel Macs with macOS 11 or later; it is signed and notarized.
See [Install (macOS)](#install-macos) for the `swy` CLI, updates and uninstalling.

## Features

### One connection model

- **Hosts** own SSH credentials once; terminals, file browsers and database connections
  tunneled "via Host" all reuse the same SSH session, so you log in once.
- **Connections sidebar** grouped by Host, with environment color dots and live status;
  drag to reorder Hosts and connections.
- **Connection editor** with a form per type, environment label, "Connect via Host" and
  *Test connection*.
- **Environment labels** (Production, Staging, Development, Local). Production is red on the
  tab, sidebar, status bar and editor border, confirms destructive statements (DROP, TRUNCATE,
  DELETE/UPDATE without WHERE, detected by parsing, not regex), and can be locked read-only.
- **Import Hosts from `~/.ssh/config`** with a preview (alias, user@host:port, auth,
  ProxyJump; already-saved entries greyed out).
- **Import connections from DBeaver** (File → Import from DBeaver…): reads every DBeaver
  workspace on the machine, or a `data-sources.json` you pick, and shows a preview first.
  PostgreSQL, SQL Server, MySQL/MariaDB, Oracle, SQLite, Snowflake, MongoDB and Redis
  connections come over with their folder, connection type (as the environment label),
  SSL mode, saved user and password (from `credentials-config.json`, stored in the
  keychain) and SSH tunnel, which becomes a Host or reuses a saved one.
- **Profiles** stored in local SQLite; JSON export and import, always without secrets.
- **Shared credentials**: one user name and password or SSH key used by many Hosts,
  database connections and remote desktops. Rotate it once and every user picks it up.
- **Secret vault**: name a secret once and type `{{vault.name}}` in any password, token or
  key field. It lives in the keychain, is read from 1Password (`op`), or is linked to Azure
  Key Vault, AWS Secrets Manager or Parameter Store and read when used, never written to disk.

### Server workspaces and remote desktops

- **Workspaces** in the title bar menu: a "Payments" or "Platform" view of your Hosts,
  connections, clusters and remote desktops, by members, tag rules and folder rules. An
  item can be in several workspaces; each one keeps its own tabs and layout.
- **Share a workspace** as a `.switchyard-workspace.json` file. Secrets stay out unless you
  choose to include them, encrypted with a passphrase; the import shows a preview and asks
  for what is missing.
- **Servers view**: every Host and remote desktop of the workspace as a list or cards, with
  OS, tags, environment, last connected and a live reachability dot; search by name, IP,
  `tag:web` or OS; group by folder, tag or environment; SSH, SFTP, Fleet, RDP / VNC per row.
- **Connect to…** (Ctrl/⌘+K): fuzzy search over every Host, desktop and connection that
  opens the right kind of session.
- **RDP and VNC** servers open in the system client (`mstsc`, Windows App, FreeRDP,
  Remmina, TigerVNC), optionally through an SSH tunnel or an RD Gateway. Passwords never
  appear on a command line.

### Databases

| Engine | Highlights |
| --- | --- |
| PostgreSQL | TLS, streaming binary decode, full type mapping, cancel (also through tunnels), transactions, catalog, plans, workload stats, HypoPG what-if |
| SQL Server | SQL login, Windows account (SSPI / NTLM), Kerberos integrated auth, Azure SQL with Entra ID (browser + PKCE, device code with MFA, password, service principal), `GO` batches, multiple result sets, cancel, showplan, workload stats, Query Store |
| Oracle | Oracle Instant Client installed and loaded at runtime, PL/SQL blocks, `q'[..]'` quoting, `DBMS_OUTPUT` as notices, cancel, catalog and `DBMS_METADATA` DDL |
| Snowflake | SQL API v2 with async polling, gzip result partitions, multi-statement requests, server-side cancel, key-pair JWT or programmatic access token, `INFORMATION_SCHEMA` catalog and `GET_DDL` |
| MySQL / MariaDB | Pure-Rust driver with TLS, multi-result scripts, `DELIMITER` scripts, warnings as notices, `KILL QUERY` cancel, catalog and `SHOW CREATE` DDL, plans (`EXPLAIN FORMAT=JSON` / `ANALYZE`), activity monitor |
| SQLite | Local database files, cancel, transactions, attached databases in the catalog, inline editing, estimated plans |
| Cloudflare D1 | REST API, SQLite dialect, type inference, catalog |
| Durable Object SQLite | The SQLite storage of one Cloudflare Durable Object through Cloudflare's `query/v2` API, so your Worker needs no change |
| MongoDB | `mongosh`-style statements (`db.orders.find(...)`), `mongodb+srv://` seed lists, documents in the grid with edits by `_id`, plans |
| Redis | Key browser in the style of Redis Insight (tree by `:` folders, type filter, TTL and size), per-type value editors, `redis-cli`-style console |

![Redis key browser with a hash open and the console below](docs/screenshots/redis.png)

All SQL engines sit behind the same `Driver` / `Dialect` traits, so quoting, LIMIT vs TOP vs
FETCH FIRST, script splitting and catalog queries are engine-aware everywhere.

### SQL editor

- Tree-sitter SQL highlighting, multi-cursor, find and replace, comment toggle, format
  (Ctrl/⌘+Shift+F), code folding.
- **Schema-aware completion**: keywords, schemas, tables, columns and functions from the
  cached catalog; aliases resolved from FROM / JOIN; recently used objects rank first.
- **Run** statement at cursor, selection, or the whole script. Splitting respects strings,
  comments, PostgreSQL dollar-quoted bodies, Snowflake `$$` bodies, Oracle PL/SQL units and
  SQL Server `GO`. Like DBeaver's "Smart" mode, a blank line between two complete
  statements also separates them, so queries without semicolons run one at a time;
  procedural blocks (DECLARE, BEGIN … END, routine bodies) stay whole.
- **Scripts**: stop at the first error or continue with the next statement, and a summary
  of statements run, failed and rows affected. Max rows per result (200 to all) sits in
  the results status bar.
- **Editing shortcuts**: toggle line / block comment (Ctrl/⌘+/, Ctrl/⌘+Shift+/), upper /
  lower case (Ctrl/⌘+Shift+X / Y), duplicate (Ctrl/⌘+Shift+D), delete line (Ctrl/⌘+D),
  move lines (Alt+Up / Down).
- **Script files**: Open SQL Script (Ctrl/⌘+O), Save (Ctrl/⌘+S) and Save As
  (Ctrl/⌘+Shift+S) on this computer; a tab remembers its file across restarts and asks
  before closing with unsaved changes.
- Large scripts stay responsive: splitting is linear and parsing for diagnostics and folds
  runs off the UI thread.
- **Parameters**: `:name`, `$1` and `@name` placeholders prompt for values, pre-filled with
  the last ones entered.
- **Diagnostics**: parse errors underlined as you type; server errors mapped to line and column.
- **Peek table**: hover or F12 on an identifier to see its columns.
- **Per-tab database / schema switcher.**
- **Transactions**: auto-commit by default, or manual mode with an open-transaction indicator
  and a warning before closing a tab with uncommitted work.
- **Query history**: statement, connection, duration and row count, searchable
  (Ctrl/⌘+Shift+H); can be turned off per connection.
- **Buffers autosave** and come back after a crash or restart.
- Drop a schema object onto the editor to insert its qualified name.
- **Snippets**: built-in and your own, per engine, offered in completion.
- Unsaved tabs show a dot and ask Save / Discard / Cancel before closing.

### Results grid

- Streams rows as they arrive; rows and columns are virtualized; typed columnar storage keeps
  1M rows × 10 numeric columns under 150 MB.
- Fetch limit (default 10,000) with *Fetch all*; Stop is always available (Ctrl/⌘+.).
- NULL styled distinctly from empty strings; numbers right-aligned.
- Column resize, reorder and pin; client-side sort and filter on loaded rows.
- **Table data** opens with a server-side WHERE filter, sort and paging bar (page size 100,
  500 or 1,000).
- Cell and range selection (Shift+click, Shift+arrows); copy as TSV.
- **Value viewer**: JSON (pretty), XML (indented and colored), text, hex, and image previews
  (PNG, JPEG, GIF, WebP, BMP, TIFF, SVG).
- **Copy / export** as CSV, JSON, Markdown and SQL INSERT statements.
- **Multiple result sets** in their own tabs; **pin** a result tab and **compare** two results.
- **Inline editing** for single-table results with a primary key: staged and highlighted edits,
  insert, duplicate and delete rows, SQL preview, committed in one transaction or discarded.
- **Foreign-key navigation**: jump from a cell to the row it references.
- Status line with row count, fetch state, elapsed time, affected rows and server notices.

### Schema explorer

- Lazy tree (loads each level on expand) with a local SQLite catalog cache; several
  connections side by side, with favorites.
- Folders per engine: tables, views, materialized views, functions, procedures, sequences,
  types, synonyms, users and roles, SQL Server Agent jobs, Oracle packages, Snowflake stages
  and tasks. Columns, keys, indexes, foreign keys and triggers under every table.
- Global object search on the server (debounced) with a local fuzzy fallback.
- Actions: open table data, generate SELECT / INSERT / UPDATE / DELETE templates from the
  real columns and primary key, **Script as** (CREATE, DROP, DROP + CREATE and more), copy
  qualified name, view DDL in a read-only tab, truncate and drop (confirmed; double-confirmed
  on Production).
- **Object properties** tab (columns, indexes, constraints, foreign keys, triggers, DDL) and
  **dependencies** (uses / used by).
- **ER diagram** laid out from foreign keys.
- **Schema compare** between two connections, databases or schemas (PostgreSQL, SQL Server,
  MySQL / MariaDB, SQLite): objects only in one side or different, per-column changes, a
  side-by-side DDL diff, and a migration script opened in a new tab, never run. Drops are
  commented out under a warning.
- **Activity monitor** with kill session (double-confirmed on Production, never available to
  agents).
- Keyboard navigation: arrows, Enter, Ctrl/⌘+C, F5 to refresh.

### Query plans and optimization

- **One plan model**: PostgreSQL JSON plans, SQL Server showplan XML, MySQL, SQLite and
  MongoDB plans all convert to one normalized operator tree.
- **Explain** (Ctrl/⌘+E) and **Explain Analyze** (Ctrl/⌘+Shift+E). Actual plans of DML run
  inside a transaction that is always rolled back.
- **Plan view**: graph colored by share of time or cost with edge width by rows, pan and zoom,
  Fit; flame (icicle) view; node detail panel; the selected node's table highlighted in the SQL.
- **Findings / hotspots**: full scan, bad estimate, rows removed by filter, spill, expensive
  nested loop, key lookup, implicit conversion, missing index; ranked and linked to the nodes,
  with configurable thresholds.
- **Plan compare and history**: plans saved with their history entry; side-by-side compare
  with time, planning, rows, pages and cost deltas and a per-operator table.
- **Workload view**: busiest statements, table and index usage, unused indexes, mostly-full-scan
  tables, missing-index DMVs and Query Store; missing extensions or permissions show a hint
  with the GRANT to run.
- **Hypothetical indexes** (PostgreSQL + HypoPG): "What if…" creates a hypothetical index,
  re-plans and opens the compare view; no real index is ever created.

### AI assistant (coding CLIs)

![Claude Code in a project folder with the folder's files and git badges beside it](docs/screenshots/coding-layout.png)

- Use the CLI you already have and are logged in to: **Claude Code**, **Codex CLI**,
  **Gemini CLI**, or a **custom** CLI defined by a command template.
- Works on every data source: SQL databases, MongoDB, Redis (read-only commands) and SSH
  Hosts, where each shell command the agent proposes runs only after you approve it in the
  app. The panel follows the tab in front.
- In the API explorer the same panel explains responses, debugs failures, reviews imports,
  writes tests, generates bodies and data, and turns a plain-language description into a new
  request.
- The whole transcript is selectable and copyable; answers and SQL cards have Copy buttons.
- **Assistant panel** (Ctrl/⌘+J): *Optimize* a statement or plan (sends the statement and
  its findings), *Plan a query*, streaming answers with tool calls, follow-ups that resume
  the CLI's session, Stop.
- **Suggestion cards** for each SQL block: index → compare a hypothetical plan, statistics →
  re-plan and compare, rewrite → compare plans; every card can open in the editor.
- **Open in terminal** starts the selected CLI interactively with the MCP server attached.
- **Code here**: the Files tab's *Code* menu (and the palette's "Claude Code in Folder…" and
  friends) starts Claude Code, Codex CLI, Gemini CLI, Aider, OpenCode or your custom CLI in a
  folder on this computer or a Host, in your own shell and login. The tab is a coding layout:
  the folder's files on the left (git badges, live refresh, click to open in the editor;
  *Files* hides them) beside the CLI, and *Changes* shows git status and diff stat in a split
  pane. *In a new git
  worktree* gives each run its own branch and folder, so several CLIs can work on one
  repository at once. These are your own sessions in your own project: no Switchyard tools
  or token are attached.
- **Agents** in the Activity pane: every running coding CLI with its state (working, waiting,
  exited), judged from its output; a toast when one behind other tabs stops to wait for you,
  and the count in the status bar.
- Installed CLIs are detected with their versions and install hints; default CLI in
  Settings → Assistant, overridable per connection.
- **Safety lives in the server**: agent access is off by default and enabled per connection;
  Production is excluded unless explicitly allowed; agents see connection names only; every
  tool is read-only (`run_query` accepts SELECT/WITH only, runs read-only with a timeout and a
  200-row cap); no DDL; each run gets a private temp directory and a short-lived, scoped
  session token; agent calls are recorded in history tagged `agent:<cli>`. Switchyard never
  reads or stores a CLI's credentials.

### `swy` CLI and MCP server

```bash
swy connections [--json]                         # saved connection names
swy query <conn> "<sql>" [--format table|csv|json]
swy explain <conn> "<sql>" [--analyze] [--open] [--format text|json]
swy workload <conn> [--json]
swy mcp                                          # MCP server on stdio
```

`swy` shares the app's core and profile store. `--open` hands the plan to the running app.
MCP tools: `list_connections`, `list_tables`, `describe_table`, `run_query`, `explain`
(estimated plans), `workload`, `what_if`, `redis_command` (read-only commands) and
`run_ssh_command` (approved in the app, one command at a time).

Every option, output format and tool limit is in the [`swy` reference](docs/cli.md). To give
your own Claude Code, Codex or Gemini session access, see
[Use Switchyard from your coding CLI](docs/agents.md).

### SSH and terminals

- Auth: password, public key (Ed25519, ECDSA, RSA; encrypted keys), keyboard-interactive (MFA)
  and SSH agent (`SSH_AUTH_SOCK`, 1Password, Windows OpenSSH agent, Pageant).
- Strict host key checking against `~/.ssh/known_hosts` (hashed entries, wildcards,
  `@revoked`) and Switchyard's own store; unknown keys prompt with the fingerprint, a changed
  key blocks the connection.
- Jump host chains (ProxyJump), keepalives, reconnect with backoff that keeps scrollback.
- GPU-rendered terminal on `alacritty_terminal`: true color, mouse reporting, bracketed paste,
  10,000-line scrollback with search, clickable links; local shell tabs.
- Up to four split panes per tab with opt-in input broadcast.
- **Session logging** to a file (plain text with escape codes stripped, or raw) with a
  file name template; **macros** recorded from keystrokes and replayed into one pane or all,
  or run on connect.
- Copy on select, right-click paste, a confirmation before pasting several lines, and
  keyword highlighting of output (all optional).
- Host folders and favorites; per-Host startup command, start folder, environment variables
  and terminal colors; duplicate a Host or bulk-edit every Host in a folder.
- **Agent forwarding** and **X11 forwarding** per Host (X server detection on Windows and macOS).

### Tunnels

- Local (`-L`), remote (`-R`) and dynamic (`-D`, SOCKS4/4a/5) forwards saved on a Host,
  optionally started when a terminal opens.
- "Via Host" database connections open a tunnel automatically on an ephemeral port, shared
  across sessions; PostgreSQL cancel uses the same tunnel.
- Tunnel manager in the status bar: direction, local port, target, status, bytes, Start / Stop.
  Stopping a tunnel ends its dependent sessions with a clear message.

### Fleet

- **Host tags** (`role:web`, `team:payments`) with an Explorer grouping by tag and a
  `tag:` filter; groups from an **Ansible inventory** (INI or YAML) become tags on import.
- **Run a command** or a saved macro on a folder, a tag or a selection of Hosts: bounded
  concurrency, per-Host timeout, cancel all, streamed output, and filters for "failed only"
  and "differs from the majority".
- **Dashboard**: uptime, load, CPU count, disk, memory, failed systemd units and OS for every
  Host from one read-only probe, refreshed on a timer, with full disks and failed units flagged.
- **Push a file or folder** over SFTP to every Host, with an overwrite policy and chmod.
- **SSH key rollout**: audit which Hosts have a key, add it, or remove it, with a backup of
  `authorized_keys` and never the key the session signed in with.
- A change on a selection that includes a Production Host needs the word PRODUCTION typed.

### File transfer

- SFTP on the Host's existing SSH session (no second login), and FTP / FTPS (explicit or
  implicit TLS, passive or active).
- Dual-pane Files tab (this computer and any Host): breadcrumbs, sortable columns, hidden-file
  toggle, multi-select, new folder, rename, delete, drag and drop between panes and from the OS.
- **Keyboard file manager**: arrows (Shift extends), Enter opens, Backspace or Alt+Up goes up,
  F2 renames, Delete deletes, F5 refreshes, Tab switches panes, Ctrl/⌘+A, Ctrl/⌘+F searches,
  Ctrl/⌘+L types a path, Ctrl/⌘+Shift+C copies paths, Ctrl/⌘+` opens a terminal in the
  folder (with its files beside it). Copy path, Open with the default app and
  Reveal in the OS file manager for the selection.
- **One pane or two**: two panes for copying between computers, one pane for browsing.
- **Preview pane** (Alt+P, or *Preview* in the command bar): the selected file beside the list
  without opening it. Text, code and configuration show their first lines with line numbers,
  images show whole, other binary files their first bytes in hex, and documents, media and
  archives a card with their details. Only the start of a file is read, so arrowing through
  a folder of large logs, locally or on a Host, stays quick; a file that changes while it is
  selected (a coding CLI editing it) updates. Drag its edge to resize it.

  ![Files tab with the preview pane showing a Rust source file](docs/screenshots/file-preview.png)

- **Git-aware and live**: folders on this computer refresh while the tab is on screen; in a
  git repository the footer shows the branch, ahead / behind and the number of changes, each
  changed file or folder gets a badge (M, A, D, R, ?, !), and *Changes* lists only what changed.
- **Transfer queue**: 4 parallel transfers, pause, resume, retry, cancel; progress, speed and
  ETA per file and overall. Interrupted transfers resume from the last byte, even after the
  app was killed.
- **Remote edit**: open a remote file in the editor; saving checks the remote modification
  time and offers Overwrite or Discard and reload on conflict.
- **Images** open in a viewer tab from any file browser; other binary files open in your
  default app through a temporary copy.

### Cloud storage and developer tools

- **Object storage** in the Files tab: Amazon S3 and S3-compatible endpoints, Cloudflare R2
  and Azure Blob Storage, with streamed multipart uploads and transfers both ways. Large
  buckets and containers load 100 entries at a time with *Load more* and a server-side
  prefix search.
- **Key / value tools**: Azure App Configuration, Azure Key Vault secrets, AWS Secrets
  Manager, AWS Parameter Store and Cloudflare Workers KV. Secrets stay hidden until shown;
  Production saves and every delete are confirmed.
- **Azure App Configuration**: keys grouped by prefix, labels and locks, a feature flag form,
  revision history with restore, label compare with copy, JSON / `.env` / kvset import and
  export, and Key Vault references resolved on request. Key Vault shows versions, dates and
  soft-deleted secrets you can recover.
- Sign in with access keys, AWS CLI profiles (including SSO), Cloudflare API tokens, Azure
  connection strings, SAS, Microsoft Entra or the Azure CLI. No cloud SDKs: small REST
  clients with their own request signing.

### Containers

- **Docker**: containers, images, volumes and networks; start, stop, restart and remove;
  inspect, streamed logs, and *Open shell* (`docker exec`) in a terminal tab.
- **Kubernetes**: context and namespace pickers, pods, deployments and services, describe and
  YAML, logs with a container picker, scale, rollout restart, port forwards with Copy URL,
  and a shell into a pod.
- **Add cluster** from AKS, GKE, EKS, Rancher or a local cluster (kind, minikube, k3d, Docker
  Desktop, Rancher Desktop). Contexts are labelled by provider, and expired sign-ins say which
  `az`, `gcloud` or `aws` command to run.
- Runs the CLIs on this computer or on a Host over its SSH session. Read-only mode, and a
  typed-name confirmation for changes on Production.

### Infra tools

- **Terraform / OpenTofu state** (read-only) from a local file, an S3 or Azure Blob backend
  with its object versions, or HCP Terraform: resources by module and type, attributes with
  sensitive values masked, outputs, dependencies, and a compare of two states.
- **Cloud inventory**: AWS (EC2, RDS, load balancers, EKS, S3), Azure Resource Graph,
  Cloudflare and Google Cloud in one filterable table; add a VM as an SSH Host or a database
  as a connection in one click.
- **Network checks**: DNS, TLS certificate (chain, SANs, days to expiry), TCP and HTTP(S)
  timing, run here or from an SSH Host, in saved lists with *Run all*.
- **Logs**: `tail -F` and `journalctl -f` on a Host, CloudWatch Logs and Azure Log Analytics,
  streamed into a filtered, level-colored view with pause, follow and export.

### Runbooks

- Step-by-step procedures with a Markdown description, parameters (secret ones stay in the
  keychain) and Note, SQL, HTTP, Shell and Confirm steps.
- `{{param}}` values are quoted per SQL dialect or shell, and secrets are bound as statement
  parameters, never inlined; you see the resolved text before each step runs.
- Run one step, run from here or run all; the run stops at the first failure, waits at
  Confirm and Shell steps, streams output and shows SQL results in a grid. Every step goes to
  history tagged `runbook`.
- Import and export as JSON, without secrets.

### API explorer

A second workspace for HTTP APIs, one click away in the title bar (or *Switch to API
Workspace* in Open anything). It shares the app's keychain, assistant and look, so you can
test the API in front of a database without opening another tool.

<table>
  <tr>
    <td width="50%"><img src="docs/screenshots/api-request.png" alt="GET request with query parameters, environment and a pretty JSON response with passing tests"></td>
    <td width="50%"><img src="docs/screenshots/api-body.png" alt="POST request with a JSON body and a 201 Created response"></td>
  </tr>
  <tr>
    <td><img src="docs/screenshots/api-import.png" alt="Import review of a Postman collection with parser findings"></td>
    <td><img src="docs/screenshots/api-runner.png" alt="Collection runner results"></td>
  </tr>
  <tr>
    <td><img src="docs/screenshots/api-load.png" alt="Load test results with latency percentiles, histogram and per-second throughput"></td>
    <td><img src="docs/screenshots/api-mocks.png" alt="Local mock server with routes, chaos settings and a call log"></td>
  </tr>
  <tr>
    <td><img src="docs/screenshots/api-contract.png" alt="Contract tab listing a removed field and a type change against earlier responses"></td>
    <td></td>
  </tr>
</table>

- **Projects** hold collections, folders and requests; rename or delete a project from the
  header menu, rename items in place (double-click or F2);
  each project reopens its request tabs. A first-run page offers Create project, Import
  collection, Paste cURL or a sample request.
- **Requests**: params, headers, body (JSON, XML, text, form, multipart, file, GraphQL),
  auth (including OAuth), variables, scripts and per-request settings; Ctrl/⌘+L focuses the
  URL, Ctrl/⌘+Enter sends.
- **WebSocket, Server-Sent Events and gRPC** requests beside HTTP: saved messages, a live
  message log with timestamps and filter, and for gRPC services from server reflection or
  `.proto` files, JSON request templates and all four streaming modes.
- **Environments** with labels (Production, Staging, Development, Local). Production is red
  on the environment chip, URL bar and request, and confirms before sending unsafe methods.
  Secret values live in the keychain or vault.
- **Import** OpenAPI 3.0 / 3.1 (JSON or YAML), Postman 2.1 collections and environments,
  Insomnia v4, HAR captures, AgentOps bundles or a cURL command. Every import is parsed and
  reviewed first (requests, folders, warnings, destructive calls flagged) and written in one
  transaction. Export collections and copy any request as cURL.
- **Scripts and tests**: `pm.*` pre-request and test scripts run in a sandboxed worker; test
  results show next to the response.
- **Runner**: run a folder or collection with CSV or JSON iteration data, a seed, a delay and
  stop on first error; describe a scenario and let the assistant generate the data.
- **Load tests**: send the open request thousands of times from up to 64 workers, by count
  or for a duration, and watch p50 / p95 / p99, throughput, error rate, status codes and a
  per-second timeline update live; set a p95 and error budget for a pass / fail verdict.
- **Mock servers**: serve a collection's saved examples on localhost with one click.
  `:id` and `{{id}}` path segments match anything, Postman's `x-mock-response-name` and
  `x-mock-response-code` headers pick an example, CORS is open for browser apps, and every
  call is logged. **Chaos** adds latency and a share of injected 500s to test a client's
  timeouts and retries; **record mode** forwards calls no example matches to the real API
  and saves what it answered as new requests and examples, so the mock fills itself.
- **Contract drift**: every JSON response is checked against the shape learned from the
  request's earlier responses and examples. Removed fields and type changes are flagged as
  breaking, new fields as additive, with no schema to write; copy the learned shape as JSON
  Schema.
- **Response panel**: Pretty (JSONPath / XPath filter) and Raw views, headers, trace, test
  results and console, with copy actions everywhere. Cookies carry between requests;
  History and Diff tabs sit beside Compose.

### Driver Manager

Everything above works with no extra installs except where a native component is needed;
then the Driver Manager detects it, explains why, installs it and retries the connection.

- Components: Kerberos / GSSAPI, Oracle Instant Client, SSH agent, X server, RDP and VNC
  clients, and coding CLIs.
- Strategies: user-level archive install into the app's data directory (no admin rights),
  package-manager command shown and run with elevation after confirmation, *Use existing path*,
  *Install from file* for offline machines, and a mirror setting.
- Signed manifests (minisign) and SHA-256 verification before extracting.
- Inline setup card in the connection editor and a Settings → Drivers page. Native libraries
  load at runtime; a missing one disables one feature and never stops the app from starting.

### Scratchpad and notes

- **Scratchpad** (Ctrl/⌘+Shift+N, or View → Scratchpad): a Notepad-style place to paste
  and jot temporary text. Scratch notes are kept as you type, survive restarts, and stay
  until you close them (closing one with text asks first). Press the shortcut again while
  the scratchpad is in front for a new scratch note.
- **Notes**, like a small local Obsidian: Ctrl/⌘+S saves a scratch note under a name as a
  markdown file in the app's data folder (`notes/<name>.md`, so other editors can open
  the folder too). Saved notes save their own edits, can be renamed or deleted, and are
  searched by name and text from the list.
- **Read** mode (Ctrl/⌘+E) renders the markdown; `[[Note name]]` links show under the
  note and open the note they name, or create it.

### Workspace and UI

- **Activity rail** with labelled Explorer, API, Tools and Activity entries, shown in both
  workspaces. The Explorer groups everything by place (servers, cloud accounts, direct
  connections) or by type (databases, terminals, files and storage, config and secrets),
  with one filter over all of it. A database's ▸ folds its schemas and tables open under
  its own row, the way a server lists its terminal and files; double-click a table for its
  data. View → Schema still shows every open database's tree on its own.
- **API** is one click away: the rail's API entry, the Tools pane's API group (requests,
  import, environments, runner, load test, mocks, history), File → New API request and the
  New chooser all open the Workbench on the right page.
- **Tools** pane: port forwards, history, workload statistics, activity monitor, snippets,
  drivers, Servers, Fleet, Containers, Runbooks, Infra tools, and one entry per cloud
  service. **Activity** pane: sessions, running queries,
  tunnels and transfers.
- Title bar menus, **Open anything** (Ctrl/⌘+P) over connections, tools and commands, and a
  **New** button (Ctrl/⌘+N) that asks what you want to connect to.
- Tabbed center area that splits right or down, inspector panel, status bar with
  connection, environment, transaction, tunnels and transfers.
- Command palette (Ctrl/⌘+Shift+P) reaches every action.
- **Updates** download in the background, are checked against their SHA-256 (and signature
  when configured), and install on restart: the AppImage is replaced in place, Windows runs
  the new installer, macOS opens the verified disk image.
- Dark and light themes. UI zoom from 70 % to 200 % (Ctrl/⌘ + `=` / `-` / `0`) scales the
  whole window, editors, grid and terminal; editor font family and size in Settings →
  Appearance.

### Security

- Passwords, passphrases and tokens live in the OS keychain (macOS Keychain, Windows Credential
  Manager, Secret Service), or an encrypted vault (argon2 + ChaCha20-Poly1305) with a master
  password when no keychain exists. Secrets never reach logs, history or exports.
- TLS verification on by default (rustls); strict SSH host keys; private keys read in place.
- If there is no keychain, see
  [Secrets and the vault](docs/troubleshooting.md#secrets-and-the-vault); if a server's
  certificate is refused, see [Certificates and TLS](docs/troubleshooting.md#certificates-and-tls).

## Keyboard shortcuts

| Action | macOS | Windows / Linux |
| --- | --- | --- |
| Command palette | ⌘⇧P | Ctrl+Shift+P |
| Quick switch | ⌘P | Ctrl+P |
| Connect to a server or connection | ⌘K | Ctrl+K |
| Run statement / script | ⌘↩ / ⌘⇧↩ | Ctrl+Enter / Ctrl+Shift+Enter |
| Explain / Explain Analyze | ⌘E / ⌘⇧E | Ctrl+E / Ctrl+Shift+E |
| Stop query | ⌘. | Ctrl+. |
| Format SQL | ⌘⇧F | Ctrl+Shift+F |
| Query history | ⌘⇧H | Ctrl+Shift+H |
| Scratchpad (again: new scratch note) | ⌘⇧N | Ctrl+Shift+N |
| New connection / query tab | ⌘N / ⌘⌥N | Ctrl+N / Ctrl+Alt+N |
| New terminal | ⌘T | Ctrl+Shift+T |
| Split terminal | ⌘D | Ctrl+Shift+D |
| Split right / down | ⌘\ / ⌘⇧\ | Ctrl+\ / Ctrl+Shift+\ |
| Toggle sidebar / assistant | ⌘B / ⌘J | Ctrl+B / Ctrl+J |
| Settings | ⌘, | Ctrl+, |
| Peek table | F12 | F12 |
| Close tab | ⌘W | Ctrl+W |
| Save (file editor, API request) | ⌘S | Ctrl+S |
| Terminal copy / paste / find | ⌘C / ⌘V / ⌘F | Ctrl+Shift+C / Ctrl+Shift+V / Ctrl+Shift+F |
| Zoom in / out / reset | ⌘= / ⌘- / ⌘0 | Ctrl+= / Ctrl+- / Ctrl+0 |

## Status

Switchyard is pre-beta; the current version is on the
[Releases page](https://github.com/jeremielumandong/switchyard/releases). Not finished yet
(see [`PLAN.md`](PLAN.md)):

- Corporate CA import and per-connection certificate pinning.
- Query plans and workload stats for Oracle and Snowflake; workload stats for MySQL.
- Terminal extras: SSH key generator, PuTTY / MobaXterm session import, Telnet, SCP, a
  shell picker, the SFTP pane following the shell's folder, Mosh.
- Schema compare of data, triggers and sequences; Kubernetes port forwards through a Host.
- Packaging: MSI/winget, `.deb`/`.rpm`/AUR, Homebrew cask, performance gates in CI.
- Updates: the app checks for new releases and shows a notice, but doesn't install them
  itself yet.
- Portable mode works with a marker file ([Configuration](docs/configuration.md#moving-the-files))
  but no portable package is published.

## Documentation

- [Configuration and file locations](docs/configuration.md): where your data lives,
  portable mode, environment variables.
- [Troubleshooting](docs/troubleshooting.md): common start-up, SSH, TLS and driver problems.
- [`swy` reference](docs/cli.md): commands, options and MCP tools.
- [Use Switchyard from your coding CLI](docs/agents.md): register `swy mcp` with Claude Code,
  Codex or Gemini.
- [Architecture](docs/ARCHITECTURE.md) and [decisions](docs/DECISIONS.md), for contributors.
- [License](LICENSE): Apache-2.0.

<a id="install-linux"></a>
## Install (Linux)

Install the latest published Linux x86_64 release:

```sh
curl -fsSL https://raw.githubusercontent.com/jeremielumandong/switchyard/main/install.sh | sh
```

The installer ([`install.sh`](install.sh)) downloads the AppImage from this repository's
GitHub Releases, checks its SHA-256 checksum, and installs it for your user:

| What | Where |
| --- | --- |
| The app | `~/Applications/Switchyard.AppImage` |
| Launcher command | `~/.local/bin/switchyard` |
| `swy` CLI | `~/.local/bin/swy` (a link to the AppImage) |
| Desktop entry and icon | `~/.local/share/applications/switchyard.desktop`, `~/.local/share/icons/hicolor` |

No Rust compiler or sudo is needed, but the AppImage uses the system's graphics stack:
a working GPU driver with the Vulkan loader, plus `libxkbcommon`, `libxkbcommon-x11`,
`libwayland-client`, `libxcb`, `fontconfig` and `freetype`. Most desktops already have
them. On Ubuntu or Debian, install any that are missing with
`sudo apt install libvulkan1 libxkbcommon0 libxkbcommon-x11-0 libwayland-client0 libxcb1 libfontconfig1 libfreetype6`.
The release is built on Ubuntu 22.04, so it needs glibc 2.35 or newer. Make sure
`~/.local/bin` is on your `PATH` so the `switchyard` and `swy` commands resolve; the
installer warns if it is not.

- **Update:** Switchyard downloads and verifies new releases itself and replaces the
  AppImage when you click *Restart to update* (Settings → General turns this off). To update
  by hand, quit Switchyard and run the same command again. The previous version is
  kept as `Switchyard.AppImage.bak`. The installer won't replace an AppImage that is
  running (it may have open transactions or unsaved edits) unless you add
  `--stop-running`: `curl -fsSL …/install.sh | sh -s -- --stop-running`.
- **Pick a release:** `curl -fsSL …/install.sh | SWITCHYARD_VERSION=v0.1.7 sh`.
- **Install somewhere else:** set `SWITCHYARD_APPIMAGE=/absolute/path/Switchyard.AppImage`.
- **Uninstall:** `curl -fsSL …/install.sh | sh -s -- --uninstall`, or
  `./install.sh --uninstall` from a checkout. Your connections, history and settings in
  `~/.config/switchyard`, and API projects, drivers and logs in `~/.local/share/switchyard`,
  are kept (delete those folders yourself to remove them; see
  [file locations](docs/configuration.md#file-locations)); passwords stay in your keychain
  or the Switchyard vault.

To build and install from a source checkout instead (needs Rust and the build packages
under [Quick start](#quick-start)):

```sh
./install.sh --build                         # build the AppImage, then install it
./install.sh dist/Switchyard-<version>-x86_64.AppImage   # install an AppImage you already have
./install.sh --uninstall
```

`packaging/linux/build-appimage.sh` only builds, into `dist/`. Other architectures than
x86_64 have no release AppImage; use `--build` there.

<a id="install-macos"></a>
## Install (macOS)

1. [Download `Switchyard-macos-universal.dmg`](https://github.com/jeremielumandong/switchyard/releases/latest/download/Switchyard-macos-universal.dmg)
   from the latest release. Its SHA-256 checksum is in `Switchyard-macos-universal.dmg.sha256`
   next to it; check it with `shasum -a 256 -c Switchyard-macos-universal.dmg.sha256`.
2. Open the disk image and drag **Switchyard** onto the **Applications** folder.
3. Start Switchyard from Launchpad, Spotlight or the Applications folder.

The app is a universal binary (Apple Silicon and Intel) for macOS 11 Big Sur or newer.
It is signed with a Developer ID and notarized by Apple, so Gatekeeper opens it without
warnings.

The `swy` CLI ships inside the app bundle. To put it on your `PATH`:

```sh
sudo mkdir -p /usr/local/bin
sudo ln -sf /Applications/Switchyard.app/Contents/MacOS/swy /usr/local/bin/swy
swy --version
```

- **Update:** quit Switchyard, download the latest DMG and drag the app to Applications
  again, replacing the old copy. The `swy` link keeps working.
- **Pick a release:** download `Switchyard-<version>-macos-universal.dmg` from that
  release on the [Releases page](https://github.com/jeremielumandong/switchyard/releases).
- **Uninstall:** quit Switchyard, move `/Applications/Switchyard.app` to the Trash and
  run `sudo rm -f /usr/local/bin/swy` if you added the link. Your connections, history and
  settings in `~/Library/Application Support/dev.Switchyard.Switchyard` are kept (delete
  that folder yourself to remove them); passwords stay in your Keychain or the Switchyard
  vault.

To build from a source checkout instead (needs Rust and Xcode's command line tools),
run `./packaging/package.sh`; it writes a universal `.dmg` and `.pkg` to `dist/`. The
`.pkg` installs to `/Applications` and links `swy` into `/usr/local/bin` for you.

<a id="install-windows"></a>
## Install (Windows)

1. [Download `Switchyard-windows-x64-setup.exe`](https://github.com/jeremielumandong/switchyard/releases/latest/download/Switchyard-windows-x64-setup.exe)
   from the latest release. Its SHA-256 checksum is in `Switchyard-windows-x64-setup.exe.sha256`
   next to it; compare it with `Get-FileHash .\Switchyard-windows-x64-setup.exe` in PowerShell.
2. Run it. It installs for your user only, into `%LOCALAPPDATA%\Programs\Switchyard`, and
   needs no administrator rights.
3. Choose the components: the app and `swy` (always), a desktop shortcut, and
   *Add swy CLI to PATH*, which puts `swy` on your user `PATH`. Open a new terminal
   afterwards so it sees the change.

Switchyard then starts from the Start menu.

- **Update:** run the setup program of the newer release. Quit Switchyard first: the
  installer force-closes a running copy to replace its files, which ends open
  transactions and can lose unsaved edits.
- **Pick a release:** download `Switchyard-<version>-windows-x64-setup.exe` from that
  release on the [Releases page](https://github.com/jeremielumandong/switchyard/releases).
- **Uninstall:** use *Uninstall Switchyard* in the Start menu, or Settings → Apps. It removes
  `swy` from your `PATH` if the installer added it. Your connections, history and settings
  in `%APPDATA%\Switchyard\Switchyard` are kept (delete that folder yourself to remove
  them; see [file locations](docs/configuration.md#file-locations)); passwords stay in
  Windows Credential Manager or the Switchyard vault.

To build the installer from a source checkout instead (needs Rust and NSIS 3), run
`pwsh packaging/windows/build-windows.ps1`; it writes `dist/*-setup.exe`.

## Quick start

```bash
cargo run -p switchyard-app          # the desktop app
cargo run -p switchyard-cli -- --help  # the `swy` CLI
cargo test --workspace               # unit tests (no network)
```

Linux builds need the usual GPUI system packages:

```bash
sudo apt install libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libxcb1-dev \
  libvulkan-dev libfontconfig-dev libfreetype-dev libssl-dev pkg-config
```

## Packaging

```bash
./packaging/package.sh                      # macOS: dist/*.dmg + *.pkg (universal); Linux: dist/*.AppImage
pwsh packaging/windows/build-windows.ps1    # Windows: dist/*-setup.exe (needs NSIS 3)
```

Each script lists its options and the signing environment variables in its header.

Integration tests run against the services in `docker/compose.yml`:

```bash
docker compose -f docker/compose.yml up -d
cargo test --workspace -- --ignored
```

## Workspace

| Crate | Responsibility |
| --- | --- |
| `switchyard-app` | GPUI application: windows, panels, editor, grid, plan view, terminal view, file browser, assistant |
| `switchyard-core` | Hosts, connections, sessions, environment rules, tokio runtime, event bus |
| `switchyard-store` | SQLite profiles, schema cache, query history, keychain / vault |
| `switchyard-db` | `Driver` / `DbSession` / `Dialect` traits, `Value`, `RowBatch`; PostgreSQL, SQL Server, MySQL, Oracle, Snowflake, SQLite, D1 / Durable Object, MongoDB and Redis drivers |
| `switchyard-remote` | SSH sessions, tunnels and forwarding, SFTP, `RemoteFs` |
| `switchyard-term` | Terminal state, local PTY |
| `switchyard-drivers` | Driver Manager: manifests, detection, install, verify, runtime loading |
| `switchyard-plan` | Plan capture, normalized plan model, findings, workload stats, what-if indexes |
| `switchyard-cli` | `swy` binary and MCP server |
| `switchyard-agents` | `AgentAdapter` trait and Claude Code / Codex / Gemini / custom adapters |
| `switchyard-api` | API workspace: collections, requests, environments, OAuth, scripts |
| `switchyard-cloud` | S3, R2 and Azure Blob as `RemoteFs`; App Configuration, Key Vault, Secrets Manager, Parameter Store, Workers KV; Terraform state, cloud inventory, cloud logs |
| `switchyard-containers` | `docker` / `kubectl` / `az` / `gcloud` / `aws` calls and their JSON for the Containers tab |
