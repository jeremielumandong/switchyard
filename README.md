# Switchyard

A fast, native, cross-platform desktop app that puts database querying, SSH terminals,
tunnels and file transfer behind one connection model. It also visualizes query plans,
finds optimization hotspots, and lets coding CLIs (Claude Code, Codex CLI, Gemini CLI or a
custom one) plan and optimize queries through a read-only MCP server.

Written in Rust on [GPUI](https://www.gpui.rs/) and
[gpui-component](https://github.com/longbridge/gpui-kit). Runs on macOS, Windows and Linux.
Licensed under Apache-2.0.

- Product and technical spec: [`docs/SPEC.md`](docs/SPEC.md)
- Build plan and progress: [`PLAN.md`](PLAN.md)
- Decisions log: [`docs/DECISIONS.md`](docs/DECISIONS.md)
- Contributor / agent guide: [`CLAUDE.md`](CLAUDE.md)
- UI design prototype: [`docs/design/Switchyard.dc.html`](docs/design/Switchyard.dc.html)

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
- **Profiles** stored in local SQLite; JSON export and import, always without secrets.

### Databases

| Engine | Highlights |
| --- | --- |
| PostgreSQL | TLS, streaming binary decode, full type mapping, cancel (also through tunnels), transactions, catalog, plans, workload stats, HypoPG what-if |
| SQL Server | SQL login, Windows account (SSPI / NTLM), Kerberos integrated auth, Azure SQL with Entra ID (browser + PKCE, device code with MFA, password, service principal), `GO` batches, multiple result sets, cancel, showplan, workload stats, Query Store |
| Oracle | Oracle Instant Client installed and loaded at runtime, PL/SQL blocks, `q'[..]'` quoting, `DBMS_OUTPUT` as notices, cancel, catalog and `DBMS_METADATA` DDL |
| Snowflake | SQL API v2 with async polling, gzip result partitions, multi-statement requests, server-side cancel, key-pair JWT or programmatic access token, `INFORMATION_SCHEMA` catalog and `GET_DDL` |
| Cloudflare D1 | REST API, SQLite dialect, type inference, catalog |

All engines sit behind the same `Driver` / `Dialect` traits, so quoting, LIMIT vs TOP vs
FETCH FIRST, script splitting and catalog queries are engine-aware everywhere.

### SQL editor

- Tree-sitter SQL highlighting, multi-cursor, find and replace, comment toggle, format
  (Ctrl/⌘+Shift+F), code folding.
- **Schema-aware completion**: keywords, schemas, tables, columns and functions from the
  cached catalog; aliases resolved from FROM / JOIN; recently used objects rank first.
- **Run** statement at cursor, selection, or the whole script. Splitting respects strings,
  comments, PostgreSQL dollar-quoted bodies, Snowflake `$$` bodies, Oracle PL/SQL units and
  SQL Server `GO`.
- **Parameters**: `:name`, `$1` and `@name` placeholders prompt for values.
- **Diagnostics**: parse errors underlined as you type; server errors mapped to line and column.
- **Peek table**: hover or F12 on an identifier to see its columns.
- **Per-tab database / schema switcher.**
- **Transactions**: auto-commit by default, or manual mode with an open-transaction indicator
  and a warning before closing a tab with uncommitted work.
- **Query history**: statement, connection, duration and row count, searchable
  (Ctrl/⌘+Shift+H); can be turned off per connection.
- **Buffers autosave** and come back after a crash or restart.
- Drop a schema object onto the editor to insert its qualified name.

### Results grid

- Streams rows as they arrive; rows and columns are virtualized; typed columnar storage keeps
  1M rows × 10 numeric columns under 150 MB.
- Fetch limit (default 10,000) with *Fetch all*; Stop is always available (Ctrl/⌘+.).
- NULL styled distinctly from empty strings; numbers right-aligned.
- Column resize, reorder and pin; client-side sort and filter on loaded rows.
- Cell and range selection (Shift+click, Shift+arrows); copy as TSV.
- **Value viewer**: JSON (pretty), XML (indented and colored), text, hex, and image previews
  (PNG, JPEG, GIF, WebP, BMP, TIFF, SVG).
- **Copy / export** as CSV, JSON, Markdown and SQL INSERT statements.
- **Multiple result sets** in their own tabs; **pin** a result tab and **compare** two results.
- **Inline editing** for single-table results with a primary key: staged and highlighted edits,
  SQL preview, committed in one transaction or discarded.
- Status line with row count, fetch state, elapsed time, affected rows and server notices.

### Schema explorer

- Lazy tree (loads each level on expand) with a local SQLite catalog cache.
- Folders per engine: tables, views, materialized views, functions, procedures, sequences,
  types, synonyms.
- Global object search on the server (debounced) with a local fuzzy fallback.
- Actions: open table data, generate SELECT / INSERT / UPDATE / DELETE templates from the
  real columns and primary key, copy qualified name, view DDL in a read-only tab, truncate and
  drop (confirmed; double-confirmed on Production).
- Keyboard navigation: arrows, Enter, Ctrl/⌘+C, F5 to refresh.

### Query plans and optimization

- **One plan model**: PostgreSQL JSON plans and SQL Server showplan XML both convert to a
  normalized operator tree.
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

- Use the CLI you already have and are logged in to: **Claude Code**, **Codex CLI**,
  **Gemini CLI**, or a **custom** CLI defined by a command template.
- **Assistant panel** (Ctrl/⌘+J): *Optimize* a statement or plan (sends the statement and
  its findings), *Plan a query*, streaming answers with tool calls, follow-ups that resume
  the CLI's session, Stop.
- **Suggestion cards** for each SQL block: index → compare a hypothetical plan, statistics →
  re-plan and compare, rewrite → compare plans; every card can open in the editor.
- **Open in terminal** starts the selected CLI interactively with the MCP server attached.
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
(estimated plans), `workload`, `what_if`.

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
- **Agent forwarding** and **X11 forwarding** per Host (X server detection on Windows and macOS).

### Tunnels

- Local (`-L`), remote (`-R`) and dynamic (`-D`, SOCKS4/4a/5) forwards saved on a Host,
  optionally started when a terminal opens.
- "Via Host" database connections open a tunnel automatically on an ephemeral port, shared
  across sessions; PostgreSQL cancel uses the same tunnel.
- Tunnel manager in the status bar: direction, local port, target, status, bytes, Start / Stop.
  Stopping a tunnel ends its dependent sessions with a clear message.

### File transfer

- SFTP on the Host's existing SSH session (no second login).
- Dual-pane Files tab (this computer and any Host): breadcrumbs, sortable columns, hidden-file
  toggle, multi-select, new folder, rename, delete, drag and drop between panes and from the OS.
- **Transfer queue**: 4 parallel transfers, pause, resume, retry, cancel; progress, speed and
  ETA per file and overall. Interrupted transfers resume from the last byte, even after the
  app was killed.
- **Remote edit**: open a remote file in the editor; saving checks the remote modification
  time and offers Overwrite or Discard and reload on conflict.

### API workspace

A second workspace (switch from the title bar) for HTTP APIs:

- Projects with collections, folders and requests; inline rename; open tabs restored per project.
- Environments with labels (Production confirms unsafe methods before sending).
- Import / export collections, paste cURL, copy as cURL; cookies; OAuth.
- `pm.*` pre-request and test scripts in a sandboxed worker; collection runs.
- Response panel with Pretty / Raw views, headers, console, trace and test results, copy actions.

### Driver Manager

Everything above works with no extra installs except where a native component is needed;
then the Driver Manager detects it, explains why, installs it and retries the connection.

- Components: Kerberos / GSSAPI, Oracle Instant Client, SSH agent, X server, and coding CLIs.
- Strategies: user-level archive install into the app's data directory (no admin rights),
  package-manager command shown and run with elevation after confirmation, *Use existing path*,
  *Install from file* for offline machines, and a mirror setting.
- Signed manifests (minisign) and SHA-256 verification before extracting.
- Inline setup card in the connection editor and a Settings → Drivers page. Native libraries
  load at runtime; a missing one disables one feature and never stops the app from starting.

### Workspace and UI

- Title bar, collapsible sidebar, tabbed center area that splits right or down, inspector
  panel, status bar with connection, environment, transaction, tunnels and transfers.
- Command palette (Ctrl/⌘+Shift+P) and quick switcher (Ctrl/⌘+P) reach every action.
- Dark and light themes.

### Security

- Passwords, passphrases and tokens live in the OS keychain (macOS Keychain, Windows Credential
  Manager, Secret Service), or an encrypted vault (argon2 + ChaCha20-Poly1305) with a master
  password when no keychain exists. Secrets never reach logs, history or exports.
- TLS verification on by default (rustls); strict SSH host keys; private keys read in place.

## Keyboard shortcuts

| Action | macOS | Windows / Linux |
| --- | --- | --- |
| Command palette | ⌘⇧P | Ctrl+Shift+P |
| Quick switch | ⌘P | Ctrl+P |
| Run statement / script | ⌘↩ / ⌘⇧↩ | Ctrl+Enter / Ctrl+Shift+Enter |
| Explain / Explain Analyze | ⌘E / ⌘⇧E | Ctrl+E / Ctrl+Shift+E |
| Stop query | ⌘. | Ctrl+. |
| Format SQL | ⌘⇧F | Ctrl+Shift+F |
| Query history | ⌘⇧H | Ctrl+Shift+H |
| New connection / query tab | ⌘N / ⌘⌥N | Ctrl+N / Ctrl+Alt+N |
| New terminal | ⌘T | Ctrl+Shift+T |
| Split terminal | ⌘D | Ctrl+Shift+D |
| Split right / down | ⌘\ / ⌘⇧\ | Ctrl+\ / Ctrl+Shift+\ |
| Toggle sidebar / assistant | ⌘B / ⌘J | Ctrl+B / Ctrl+J |
| Settings | ⌘, | Ctrl+, |
| Peek table | F12 | F12 |

## Status

Switchyard is pre-beta (v0.1.6). Not finished yet (see [`PLAN.md`](PLAN.md)):

- FTP / FTPS, corporate CA import and per-connection certificate pinning.
- Query plans and workload stats for Oracle and Snowflake; in-app approval for agent actual plans.
- Explorer: per-relation column/index children, object properties tab, script-as, server-side
  paging, foreign-key navigation, ER diagram, activity monitor, snippets.
- Terminal extras (logging, macros, key generator, PuTTY/MobaXterm import, Telnet, SCP).
- Packaging: MSI/winget, `.deb`/`.rpm`/AUR, Homebrew cask, auto-update, portable mode,
  performance gates in CI.

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
| `switchyard-db` | `Driver` / `DbSession` / `Dialect` traits, `Value`, `RowBatch`; PostgreSQL, SQL Server, Oracle, Snowflake and D1 drivers |
| `switchyard-remote` | SSH sessions, tunnels and forwarding, SFTP, `RemoteFs` |
| `switchyard-term` | Terminal state, local PTY |
| `switchyard-drivers` | Driver Manager: manifests, detection, install, verify, runtime loading |
| `switchyard-plan` | Plan capture, normalized plan model, findings, workload stats, what-if indexes |
| `switchyard-cli` | `swy` binary and MCP server |
| `switchyard-agents` | `AgentAdapter` trait and Claude Code / Codex / Gemini / custom adapters |
| `switchyard-api` | API workspace: collections, requests, environments, OAuth, scripts |
