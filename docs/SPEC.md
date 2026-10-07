# Switchyard — Product & Technical Spec

Oct 5, 2026 · @Jeremie

## Overview

Switchyard is a fast, native, cross-platform desktop app that puts database querying (PostgreSQL, SQL Server), SSH terminals and file transfer (SFTP, FTP, FTPS) behind one connection model. It is written in Rust on GPUI and gpui-component.

**Goals**

- One place to reach a server: a saved Host opens a terminal, a file browser, and database connections tunneled through it.
- Feel instant: fast launch, instant tabs, and smooth scrolling through a million-row result (see Performance targets).
- An editor good enough to write SQL all day: schema-aware completion, statement-at-cursor execution, formatting, history.
- Zero-friction setup: if a driver or system component is missing, Switchyard detects it and installs it for the user.

**Non-goals for v1:** ER modeling, migration tooling, cloud or team sync, NoSQL databases, and general-purpose code editing.

**Target users:** platform engineers, backend developers and DBAs who today juggle a DB client, a terminal app and an FTP client.

## Scope

v1 ships PostgreSQL, SQL Server, Oracle, Snowflake, Cloudflare D1, SSH and file transfer on macOS, Windows and Linux. Oracle connects through Oracle Instant Client, installed and loaded at runtime by the Driver Manager.

| Area | v1 | Later |
| --- | --- | --- |
| Databases | PostgreSQL, SQL Server, Oracle, Snowflake, Cloudflare D1 | MySQL/MariaDB, SQLite |
| Remote access | SSH terminal, local shell, jump hosts, local, remote and dynamic (SOCKS) forwarding, agent and X11 forwarding, Telnet and raw TCP, external Mosh/RDP/VNC viewers (M7) | Embedded RDP/VNC, serial ports |
| File transfer | SFTP, FTP, FTPS, transfer queue, remote file edit | Folder sync, S3-compatible storage |
| Editor | SQL highlighting, schema completion, run statement/selection/script, format, history | Explain-plan viewer, snippet library |
| Data | Virtualized grid, inline edit with staged commit, export CSV/JSON/SQL | Import wizard, data compare |
| Setup | Driver Manager with automatic install of missing components | Enterprise mirror, driver packs |

## Core concepts

Everything hangs off the Host: one saved machine owns the SSH credentials that terminals, file browsers and tunneled database connections reuse.

| Object | What it is | Key fields |
| --- | --- | --- |
| Host | A machine you reach over SSH | Address, port, user, auth method, jump host chain, environment label, color, tags |
| DB connection | A database endpoint, optionally reached through a Host | Engine, server, port, database, auth, "via Host" tunnel, read-only flag |
| File connection | SFTP (uses its Host's SSH session) or FTP/FTPS (own credentials) | Protocol, TLS mode, passive/active, default remote path |
| Terminal profile | A shell to open on a Host or locally | Shell, startup command, environment variables |
| Session | A live, open instance of any connection | Owns sockets, tunnels and cancel handles; tabs bind to sessions |
| Workspace | A saved layout | Open tabs, unsaved editor buffers (autosaved), pinned connections |

**Environment labels** (Production, Staging, Development, Local) drive safety and color. Production connections show a red accent everywhere, confirm destructive statements (DROP, TRUNCATE, DELETE or UPDATE without WHERE), and can be locked read-only.

**Storage:** profiles live in a local SQLite file in the platform config directory. Secrets live in the OS keychain. Profiles export and import as JSON, always without secrets, and Hosts can be imported from `~/.ssh/config`.

## Driver Manager and automatic setup

A fresh install connects to PostgreSQL, SQL Server, SSH, SFTP and FTP with nothing else installed, because every v1 protocol uses a pure-Rust driver compiled into the app. The Driver Manager handles the cases that need an extra native component: it detects what is missing, installs it for the user, verifies it, and retries the connection.

**Rule:** optional native libraries are loaded at runtime (via `libloading`), never linked at build time. A missing library disables one feature; it never stops the app from starting.

### Components it manages

| Component | Needed for | Windows | macOS | Linux |
| --- | --- | --- | --- | --- |
| Kerberos / GSSAPI | SQL Server integrated (Active Directory) auth | Built in (SSPI) | Built in | Install via package manager (apt, dnf, pacman, zypper) |
| SSH agent | Agent-based SSH auth | Enable OpenSSH agent service, or detect Pageant | Built in | Detect `SSH_AUTH_SOCK`; offer to start an agent |
| Corporate CA certificates | TLS to servers with internal certificates | Import from Windows cert store | Import from Keychain | Import from system bundle or a file |
| Oracle Instant Client | Oracle connections | Vendor archive (zip) into app directory | Guided: Oracle ships a .dmg; point *Use existing path* at the installed folder | Vendor archive (zip) into app directory; needs the system `libaio` |

### Setup flow

1. **Detect.** When a connection opens (or from Settings → Drivers), check each required component against its manifest: known install paths, environment variables, the app-managed directory, and minimum version.
2. **Explain.** If something is missing, show an inline card in the connection dialog: what is missing, why it is needed, download size, license. Actions: *Install automatically*, *Use existing path*, *Show manual steps*.
3. **Install.** Prefer a user-level install into the app-managed directory (`<data_dir>/switchyard/drivers/<component>/<version>/`), which needs no admin rights. When only a system package works, show the exact command (winget, Homebrew, apt, dnf, pacman) and run it after confirmation through the platform's elevation prompt.
4. **Verify.** Check the download's SHA-256 against a signed manifest (minisign/ed25519) fetched from the Switchyard update server. Refuse and report on any mismatch.
5. **Activate.** Load the library, register it, and retry the original connection automatically.
6. **Maintain.** Settings → Drivers lists each component with version, status, location, update available, and Remove.

Components with click-through licenses (Oracle) show the license and require acceptance before download. For offline or locked-down machines, *Install from file* accepts a pre-downloaded archive, and an enterprise setting can point downloads at an internal mirror.

### Manifest entry (example)

```json
{
  "id": "gssapi",
  "required_by": ["mssql.integrated_auth"],
  "detect": { "libraries": ["libgssapi_krb5.so.2"] },
  "platforms": {
    "windows": { "strategy": "builtin" },
    "macos":   { "strategy": "builtin" },
    "linux":   { "strategy": "package",
                 "packages": { "apt": "libgssapi-krb5-2", "dnf": "krb5-libs", "pacman": "krb5" } }
  }
}
```

## SQL editor

The editor is the gpui-component code editor with a tree-sitter SQL grammar, plus in-process completion and diagnostics driven by the cached schema. No external language server is needed.

- **Highlighting:** tree-sitter SQL grammar with per-dialect keyword sets (PostgreSQL, T-SQL).
- **Completion:** keywords, schemas, tables, columns, functions and procedures from the cached catalog. Aliases resolve from FROM and JOIN clauses. Ranking favors recently used objects.
- **Execution:** run statement at cursor (Ctrl/Cmd+Enter), run selection, run whole script (Ctrl/Cmd+Shift+Enter). Script splitting follows dialect rules: `;` for PostgreSQL including dollar-quoted bodies, `GO` batch separators for SQL Server.
- **Multiple result sets** each open in their own results tab under the editor.
- **Parameters:** `:name`, `$1` and `@name` placeholders prompt for values before running.
- **Diagnostics:** parse errors from `sqlparser-rs` underlined as you type. Server errors map to a line and column (PostgreSQL returns a character position; SQL Server returns a line number).
- **Editing:** multi-cursor, find and replace, bracket matching, comment toggle, format document, code folding.
- **History:** every executed statement is stored with connection, duration and row count, and is searchable. History can be turned off per connection.
- **Buffers:** autosaved continuously, restored after a crash or restart.
- **Transactions:** auto-commit by default; a manual transaction mode shows an open-transaction indicator in the status bar and warns before closing a tab with uncommitted work.

## Results grid

The grid streams rows as they arrive and draws only the visible cells, so a million-row result scrolls as smoothly as ten rows.

- **Virtualization:** rows and columns are both virtualized; only cells in view are laid out and painted.
- **Streaming:** the first batch renders as soon as it arrives. Fetching continues in the background up to a configurable limit (default 10,000 rows), with a *Fetch all* action.
- **Storage:** typed columnar buffers per column; strings and byte values in an arena rather than one allocation per cell.
- **Large values:** cells truncate for display; a value viewer opens JSON (pretty), XML, text, hex for binary, and image previews.
- **NULL** renders visually distinct from an empty string.
- **Interaction:** cell and range selection, column resize, reorder and pin, client-side sort and filter on loaded rows, or re-run with ORDER BY.
- **Copy and export:** TSV, CSV, JSON, Markdown, and SQL INSERT statements; export to file for full results.
- **Inline editing:** for single-table results with a primary key. Edits are staged and highlighted, previewed as SQL, then committed in one transaction or discarded.
- **Status line:** row count, fetch state, elapsed time, affected rows, server notices.
- **Cancel:** a Stop control is always available while a query runs (PostgreSQL cancel request, SQL Server attention signal).

## Schema explorer

The explorer loads each tree level only when it is expanded and caches the catalog locally, so connecting never waits on a full introspection.

- **Tree (PostgreSQL):** Connection → Database → Schema → Tables, Views, Materialized views, Functions, Procedures, Sequences, Types.
- **Tree (SQL Server):** Connection → Database → Schema → Tables, Views, Stored procedures, Functions, Synonyms.
- **Cache:** catalog stored in local SQLite, refreshed on demand or when DDL runs in the same session.
- **Object detail:** columns with types and nullability, indexes, constraints, foreign keys, triggers, estimated row count, generated DDL.
- **Actions:** open table data (first 100 rows), generate SELECT, INSERT or UPDATE templates, copy qualified name, view DDL. Truncate and Drop require confirmation, and a second confirmation on Production.
- **Search:** fuzzy search across all cached objects in a connection.

## SSH terminal and tunnels

One SSH session per Host is shared by its terminals, SFTP browser and database tunnels, so the user authenticates once.

**Connection and auth** (via `russh`)

- Password, public key (OpenSSH key formats: Ed25519, ECDSA, RSA), keyboard-interactive for MFA prompts, and SSH agent.
- Host key verification: reads `~/.ssh/known_hosts` and Switchyard's own store. Unknown keys prompt with the fingerprint; a changed key blocks the connection with a clear warning.
- Jump host chains (ProxyJump), keepalives, and automatic reconnect with backoff.
- Import Hosts from `~/.ssh/config`.

**Terminal**

- Terminal state from `alacritty_terminal`, rendered on the GPU through GPUI. Local shell tabs use a pseudo-terminal (`portable-pty`).
- True color, mouse reporting, bracketed paste, scrollback (default 10,000 lines), search in scrollback, clickable links.
- Split panes inside a terminal tab, with opt-in broadcast of input to all panes.

**Tunnels**

- Port forwards saved on a Host: local (`-L`), remote (`-R`, the Host listens) and dynamic (`-D`, a SOCKS4/5 proxy), each optionally started when a terminal connects to the Host. A DB connection set to "via Host" opens its tunnel automatically on an ephemeral local port.
- Tunnels are shared across sessions that need them and reconnect with the Host.
- A tunnel manager lists active tunnels with local port, target, status and bytes transferred, and can stop them.

## File transfer

SFTP rides the Host's existing SSH session (no second login); FTP and FTPS use their own credentials. Both share one dual-pane browser and one transfer queue.

- **Protocols:** SFTP via `russh-sftp`; FTP and FTPS (explicit and implicit TLS) via `suppaftp`, passive and active modes.
- **Browser:** dual pane, local on the left and remote on the right. Drag and drop between panes and from the OS file manager. Breadcrumb path bar, sortable columns, hidden-file toggle.
- **Transfer queue:** parallel transfers (default 4), pause, resume and retry. Interrupted transfers resume from the last byte (SFTP offsets, FTP REST). Shows progress, speed and ETA per file and overall.
- **File operations:** rename, delete, new folder, permissions (SFTP), properties.
- **Remote edit:** open a remote file in the Switchyard editor; saving uploads it back after checking the remote modification time for conflicts.

## Architecture

The UI thread never touches the network: all I/O runs on a tokio runtime and talks to the GPUI app through channels, so a slow server can never freeze the window.

&#91;embedded content: Switchyard architecture: UI thread, tokio runtime, external systems\]

The UI sends commands and receives events over channels; database connections reach servers directly or through an SSH tunnel owned by the remote layer.

### Workspace crates

| Crate | Responsibility |
| --- | --- |
| `switchyard-app` | GPUI application: windows, panels, editor, grid, terminal view, file browser |
| `switchyard-core` | Hosts, connections, sessions, environment rules, event bus between UI and runtime |
| `switchyard-store` | SQLite profile store, schema cache, query history; keychain access via `keyring` |
| `switchyard-db` | `Driver` and `Dialect` traits, shared `Value` type; `pg` (tokio-postgres) and `mssql` (tiberius) modules |
| `switchyard-remote` | SSH sessions, tunnels, SFTP (`russh`, `russh-sftp`), FTP/FTPS (`suppaftp`) |
| `switchyard-term` | Terminal state (`alacritty_terminal`), local PTY (`portable-pty`) |
| `switchyard-drivers` | Driver Manager: manifests, detection, download, signature check, runtime loading |

### Driver contract

Dialect-specific behavior (identifier quoting, catalog queries, LIMIT vs TOP, batch separators) lives behind `Dialect`, so further engines slot in without touching the UI.

```rust
#[async_trait]
pub trait Driver: Send + Sync {
    fn engine(&self) -> Engine;
    fn dialect(&self) -> &dyn Dialect;
    fn requirements(&self, cfg: &DbConfig) -> Vec<ComponentId>; // checked by the Driver Manager
    async fn connect(&self, cfg: &DbConfig, via: Option<TunnelEndpoint>) -> Result<Box<dyn DbSession>>;
}

#[async_trait]
pub trait DbSession: Send {
    async fn execute(&mut self, sql: &str, params: &[Value]) -> Result<ResultStream>;
    fn cancel_handle(&self) -> CancelHandle;
    async fn introspect(&mut self, scope: IntrospectScope) -> Result<CatalogChunk>;
    async fn begin(&mut self) -> Result<()>;
    async fn commit(&mut self) -> Result<()>;
    async fn rollback(&mut self) -> Result<()>;
}

// ResultStream yields: Columns(meta) · Rows(RowBatch) · Notice(text) · NextResultSet · Done { affected, elapsed }
```

## Performance targets

These are proposed release gates for v1, measured on a mid-range laptop; each needs a benchmark in CI before beta.

| Metric | Target |
| --- | --- |
| Cold start to usable window | Under 500 ms |
| Keystroke to rendered frame in the editor | Under 8 ms |
| First rows visible after the server sends them | Under 50 ms |
| Grid scrolling with 1M rows loaded | No dropped frames at the display refresh rate |
| Memory, idle with three tabs open | Under 150 MB |
| Memory for 1M rows × 10 numeric columns | Under 150 MB |
| Query cancel to UI idle | Under 200 ms after the server acknowledges |
| Terminal: `cat` of a 100 MB file | UI stays responsive throughout |
| SFTP throughput | At least 90% of OpenSSH `sftp` on the same link |

Any operation under 200 ms shows no spinner; longer ones show progress in place, never a blocking modal.

## Security and credentials

Secrets never touch disk in plain text: they live in the OS keychain, and nothing Switchyard logs, exports or syncs contains them.

- **Secret storage:** `keyring` crate (macOS Keychain, Windows Credential Manager, Linux Secret Service). When no secret service exists, as on some minimal Linux setups, fall back to an encrypted local vault unlocked by a master password.
- **Never persisted in plain text:** passwords, passphrases and tokens stay out of logs, query history, crash reports and profile exports.
- **TLS:** `rustls` with certificate verification on by default. Per-connection certificate pinning ("trust this certificate") and corporate CA import through the Driver Manager.
- **SSH:** strict host key checking as described above; private keys are read in place, never copied into Switchyard storage.
- **Production guards:** destructive-statement confirmation, optional read-only lock, red environment accent.
- **Optional auto-lock:** after idle time, require OS authentication or the master password before secrets are used again.
- **Supply chain:** Driver Manager downloads are signed and checksummed; app updates are signed.
- **Telemetry:** off by default; crash reports are opt-in and scrubbed of SQL text and hostnames.

## Cross-platform and packaging

One codebase builds for macOS, Windows and Linux from a CI matrix; platform differences are confined to packaging, keychain backends and the Driver Manager's install strategies.

| Platform | Packages | Platform notes |
| --- | --- | --- |
| macOS | Signed and notarized `.dmg`, Homebrew cask | Universal binary (Apple Silicon and Intel) |
| Windows | Signed MSI, winget | SSPI for SQL Server integrated auth; OpenSSH agent and Pageant detection |
| Linux | AppImage, `.deb`, `.rpm`, AUR | Wayland and X11; Secret Service or encrypted-vault fallback |

- **Paths:** config, data and cache directories from the `directories` crate, following each platform's conventions.
- **Portable mode:** a marker file next to the binary keeps all config and drivers beside it.
- **Updates:** signed in-app updates on macOS and Windows; Linux defers to the package manager when installed from one.
- **CI:** GitHub Actions matrix builds, runs tests against PostgreSQL and SQL Server containers and an SSH/FTP test server, and runs the performance benchmarks.

## UI design brief (for Claude Design)

Design the desktop UI for Switchyard: a dense, keyboard-first workspace where database querying, SSH terminals and file transfer share one window and one connection model. This section is self-contained and can be handed to a designer as is.

### Product in one paragraph

Switchyard is a native desktop app for engineers who work on servers and databases all day. A saved **Host** (a server) can open terminals, a file browser, and database connections tunneled through it. Supported in v1: PostgreSQL, SQL Server, SSH, SFTP, FTP/FTPS. It runs on macOS, Windows and Linux and is built with GPUI and gpui-component (a shadcn/ui-inspired component set), so designs should use that vocabulary: buttons, inputs, selects, tabs, tree views, tables, dock panels, popovers, dialogs, toasts.

### Design principles

- **Dense but calm.** Show a lot of information without visual noise: thin borders, restrained color, compact rows (24–28 px).
- **Keyboard-first.** Every action reachable from the command palette; shortcuts shown in menus and tooltips.
- **Speed you can see.** No spinners for short operations; streaming results appear progressively; progress shown in place, never as blocking modals.
- **Environment is unmistakable.** Production is always red, everywhere the connection appears.
- **Native feel on each platform.** Respect platform title bars and window controls; one consistent layout inside.

### Main window layout

1. **Title bar:** workspace name and switcher, command palette trigger, global search.
2. **Left sidebar (collapsible):** Connections tree grouped by Host or by folder, each with environment color dot and live status. Below it (or as a second tab), the Schema explorer for the active database connection.
3. **Center work area:** tabs, splittable horizontally and vertically. Tab types: SQL editor, Terminal, Files, Table data, Object DDL. Each tab shows its connection and environment color.
4. **Right panel (optional, collapsible):** inspector for the current selection: value viewer, table details, transfer details.
5. **Status bar:** active connection, environment badge, transaction state, active tunnels, background transfers, row/time info for the focused result.

### Screens and states to design

| Screen | Contents | States to show |
| --- | --- | --- |
| Welcome / empty workspace | Recent connections, New Host, New Connection, Import from `~/.ssh/config` | First run (nothing saved), returning user |
| Connection editor (dialog) | Type picker (PostgreSQL, SQL Server, SSH Host, SFTP, FTP/FTPS); fields per type; "Connect via Host" picker; environment label; Test connection | Empty, filled, testing, test passed, test failed with error, missing driver |
| Driver setup card | What is missing, why, size, license; Install automatically / Use existing path / Show manual steps | Missing, downloading with progress, verifying, installed, failed with retry, needs admin (shows exact command), license acceptance |
| SQL editor tab | Editor on top, results below (resizable split), toolbar with Run, Stop, transaction mode, connection picker | Idle, running, completion popup open, inline error, open transaction |
| Results grid | Virtualized grid, result-set tabs, status line, export menu | Streaming, complete, empty, error, cancelled, editing with staged changes and SQL preview |
| Value viewer | JSON, XML, text, hex, image | Each format, very large value |
| Schema explorer | Lazy tree, object search, context menu | Loading node, cached, refreshing |
| Terminal tab | GPU terminal, split panes, search bar | Connected, reconnecting, disconnected, host key prompt, host key changed (blocking warning) |
| Files tab | Dual pane local and remote, breadcrumbs, toolbar; transfer queue drawer at the bottom | Browsing, drag in progress, transfers running, paused, failed, conflict on remote edit |
| Tunnel manager | List of tunnels with local port, target, status, bytes | Active, reconnecting, failed |
| Command palette and quick switcher | Fuzzy list of actions or connections with shortcuts | Empty query, results, no results |
| Production safety dialog | The statement, affected object, explicit confirm | Destructive statement on Production |
| Settings | General, Editor, Appearance, Keybindings, Drivers, Security | Drivers page listing installed components and updates |

### Visual system

- **Themes:** dark (primary) and light, plus a monospace editor font setting.
- **Environment colors:** Production red, Staging amber, Development green, Local neutral gray. Used on tab edges, sidebar dots, status bar badge and editor border.
- **Data colors:** NULL in a muted italic style; staged edits highlighted; numbers right-aligned in tabular figures.
- **Icons:** one line icon set; a distinct icon per connection type (PostgreSQL, SQL Server, SSH, SFTP, FTP).

### Key shortcuts to reflect in the design

| Action | Shortcut (macOS / Windows and Linux) |
| --- | --- |
| Command palette | Cmd+Shift+P / Ctrl+Shift+P |
| Quick switch connection | Cmd+P / Ctrl+P |
| Run statement at cursor | Cmd+Enter / Ctrl+Enter |
| Run script | Cmd+Shift+Enter / Ctrl+Shift+Enter |
| Stop query | Cmd+. / Ctrl+. |
| New terminal on current Host | Cmd+T / Ctrl+Shift+T |

### Deliverables wanted

The main window in dark and light themes, every screen in the table above with its listed states, the connection editor for each connection type, and the component set (tabs, tree rows, grid cells, status bar items, environment badges) as reusable pieces.

## Milestones

Each milestone ends with a working, usable app; PostgreSQL plus the editor and grid come first because they decide whether Switchyard feels fast.

1. **M0 Skeleton.** Cargo workspace, GPUI window and dock layout, profile store, keychain access, command palette.
   - Exit: create and save a connection profile; secrets land in the keychain.
2. **M1 PostgreSQL, editor, grid.** tokio-postgres driver, SQL editor with completion, streaming virtualized grid, cancel, history.
   - Exit: query a 1M-row table, scroll without dropped frames, cancel a long query.
3. **M2 SSH and tunnels.** russh sessions, known\_hosts, jump hosts, `~/.ssh/config` import, terminal tabs, local shell, PostgreSQL via Host tunnel.
   - Exit: open a terminal and a tunneled DB connection to the same Host with one login.
4. **M3 SQL Server and Driver Manager.** tiberius driver, `GO` batches, multiple result sets; Driver Manager with GSSAPI auto-setup for integrated auth.
   - Exit: integrated auth works on a Linux machine that started without Kerberos libraries.
5. **M4 File transfer.** SFTP on the Host session, FTP/FTPS, dual-pane browser, transfer queue with resume, remote edit.
   - Exit: resume an interrupted 1 GB upload.
6. **M5 Packaging and beta.** Signed builds for all three platforms, auto-update, performance gates in CI.
   - Exit: every performance target passes on all three platforms.

Oracle moved before beta (user decision, 2026-10-07): Instant Client auto-install through the Driver Manager.

## Open questions

- [ ] License and business model: open source (which license?) or a commercial Incubarity product? This also decides whether GPL code such as Zed's editor crate can be used.
- [ ] Which SQL Server auth modes ship in v1: SQL login and integrated auth only, or Microsoft Entra ID tokens too?
- [ ] Should profiles sync across machines, and if so through what (file in a synced folder, or a service)?
- [ ] Default row fetch limit: 10,000, or configurable per connection from the start?
- [ ] Crash reporting provider, if crash reports are offered.
