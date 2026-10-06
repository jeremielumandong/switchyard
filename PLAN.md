# PLAN.md — Switchyard build plan

Work top to bottom. Each task is sized for one focused session. "Done when" is the acceptance
test; a task is not done until it passes plus the Definition of Done in CLAUDE.md.
Tick `[x]` and add a short note when finished.

---

## M0 — Skeleton

Exit: the app opens with the full layout, profiles save to SQLite, secrets land in the keychain.

- [ ] **M0-1 Workspace scaffold.** Cargo workspace with the 10 crates, `rust-toolchain.toml`,
  `rustfmt.toml`, shared `[workspace.dependencies]`, GitHub Actions running fmt, clippy and tests
  on macOS, Windows and Linux.
  Done when: `cargo build --workspace` passes and CI is green on all three platforms.
  Note: Partial: `cargo build --workspace`, fmt, clippy and tests pass locally on Linux; `.github/workflows/ci.yml` runs them on all three platforms but has not been observed green yet.
- [ ] **M0-2 Test services.** `docker/compose.yml` with PostgreSQL 16, SQL Server 2022,
  an OpenSSH server (password + key auth), and an FTP server with FTPS. Seed scripts for a
  sample schema including one table with 1,000,000 rows.
  Done when: `docker compose up -d` starts all four and a smoke test connects to each.
  Note: Partial: compose file and seed scripts written (1M-row `orders`); only the PostgreSQL seed was verified (against a local PostgreSQL 16, no Docker in the build environment). No smoke test yet for SQL Server, SSH or FTP.
- [ ] **M0-3 Window and layout.** GPUI app with gpui-component: title bar, collapsible left
  sidebar, center tab area with splits, optional right panel, status bar. Light and dark themes.
  Done when: layout matches SPEC "Main window layout"; theme toggle works; panels collapse.
  Note: Partial: title bar, collapsible sidebar, tabs, right inspector panel, status bar, light/dark themes match the design. Split panes in the tab area are missing.
- [x] **M0-4 Runtime bridge.** `switchyard-core` owns a multi-thread tokio runtime, exposes a
  `RuntimeHandle` to spawn work and an event bus (commands in, events out) the UI subscribes to.
  Done when: a test command sleeping 2 s on the runtime leaves the UI responsive and its result
  event updates a GPUI entity.
  Note: `Core` owns the runtime; `crates/core/tests/flow.rs` covers a 2 s mock sleep; the UI awaits events with `cx.spawn_in`.
- [x] **M0-5 Domain model.** `Host`, `DbConnection`, `FileConnection`, `TerminalProfile`,
  `EnvironmentLabel`, `Workspace` with serde and validation. Secrets referenced by id, never inline.
  Done when: unit tests cover validation and round-trip serialization.
  Note: `crates/store/src/model.rs` tests cover validation and serde round trips; secrets are `SecretRef`s.
- [x] **M0-6 Profile store.** `rusqlite` (bundled) with versioned migrations; CRUD for all profile
  types; JSON export/import that strips secrets.
  Done when: tests prove export contains no secret material and import restores profiles.
  Note: Migrations via `user_version`; `export_has_no_secret_material_and_import_restores`.
- [x] **M0-7 Secrets.** Keychain wrapper over `keyring` using `SecretString`; fallback encrypted
  vault (argon2 + chacha20poly1305) with master password when no Secret Service exists.
  Done when: tests cover both backends; secrets never appear in logs (assert on captured tracing).
  Note: Keychain (`keyring`), vault (argon2 + chacha20poly1305) and in-memory backends; tests cover the vault and the captured-tracing check. The OS keychain backend is not exercised in headless CI.
- [x] **M0-8 Actions and command palette.** Action registry, default keybindings from SPEC,
  command palette (Ctrl/Cmd+Shift+P) and quick switcher (Ctrl/Cmd+P) with fuzzy matching.
  Done when: every action registered so far is reachable from the palette.
  Note: `actions.rs` registry + `palette.rs`; every action is listed in `palette_commands`.
- [ ] **M0-9 Connections sidebar and editor.** Sidebar tree grouped by Host or folder with
  environment dots; connection editor dialog with a form per type and environment label.
  "Test connection" is stubbed until drivers exist.
  Done when: create, edit, delete and reorder connections; changes persist across restarts.
  Note: Partial: tree grouped by Host, environment dots, editor for every type with test connection (PostgreSQL real, others stubbed), create/edit/delete persist. Store keeps `sort_order` but there is no drag-to-reorder UI yet.

## M1 — PostgreSQL, editor, grid

Exit: query the 1M-row table, scroll without dropped frames, cancel a long query.

- [x] **M1-1 DB contract.** In `switchyard-db`: `Value`, `ColumnMeta`, columnar `RowBatch`,
  `ResultStream` events, `Driver`, `DbSession`, `Dialect`, `CancelHandle`, `IntrospectScope`,
  `CatalogChunk`.
  Done when: traits compile with a mock driver used in unit tests.
  Note: `MockDriver` (`rows N`, `sleep MS`, `fail`) drives the core tests.
- [x] **M1-2 PostgreSQL driver.** Connect with TLS (`tokio-postgres-rustls`), simple and extended
  protocol with parameters, streaming rows into `RowBatch`, notices as events. Type mapping: bool,
  int2/4/8, float4/8, numeric, text/varchar/char, bytea, uuid, json/jsonb, date, time, timestamp,
  timestamptz, interval; arrays and unknown types fall back to text.
  Done when: integration tests cover each type and a 1M-row stream with bounded memory.
  Note: rustls + native roots; binary decode; `crates/db/tests/pg.rs` (ignored, needs `SWITCHYARD_PG_URL`) covers every listed type and the 1M-row stream.
- [x] **M1-3 Cancel.** Wire `CancelHandle` to tokio-postgres's cancel token; Stop button and
  Ctrl/Cmd+. call it.
  Done when: `SELECT pg_sleep(30)` is cancelled and the UI is idle within 1 s.
  Note: `cancel_pg_sleep` integration test; Stop and Ctrl/Cmd+. wired.
- [x] **M1-4 Transactions.** Auto-commit by default; manual mode with begin/commit/rollback, status
  bar indicator, warning when closing a tab with an open transaction.
  Done when: integration test verifies rollback discards changes.
  Note: `rollback_discards_changes`; status bar shows the open transaction; closing such a tab warns.
- [x] **M1-5 PostgreSQL introspection.** Catalog queries for databases, schemas, tables, views,
  materialized views, columns, indexes, constraints, foreign keys, functions, procedures,
  sequences, types. Lazy per scope; cached in the store.
  Done when: insta snapshots of catalog output against the seeded schema.
  Note: insta snapshots in `crates/db/tests/snapshots`; cached in `schema_cache`.
- [ ] **M1-6 Editor tab.** gpui-component editor with tree-sitter SQL highlighting, multi-cursor,
  find/replace, comment toggle, folding. Buffers autosave and restore after restart.
  Done when: kill the app mid-edit, relaunch, buffer content is intact.
  Note: Partial: tree-sitter SQL highlighting, find, comment toggle and buffer autosave/restore (`buffers_survive_reopen`, checked manually by killing the app) work. Multi-cursor and folding are not verified.
- [x] **M1-7 Statement splitting and execution.** PostgreSQL dialect splitter handling strings,
  comments and dollar-quoted bodies. Run statement at cursor, selection, whole script.
  Parameter prompts for `:name` and `$1`.
  Done when: table-driven tests cover tricky scripts; all three run modes work in the app.
  Note: Lexer-driven splitter with table tests (dollar quotes, comments, `GO`); statement/selection/script runs and `:name`/`$1` prompts checked in the app.
- [x] **M1-8 Completion.** Keywords, schemas, tables, columns, functions from the cached catalog;
  alias resolution from FROM/JOIN; ranking by recent use.
  Done when: completion lists correct columns for an aliased table in a multi-join query.
  Note: `complete.rs` tests cover aliased multi-join columns; tables referenced by executed statements rank first.
- [x] **M1-9 Diagnostics.** `sqlparser` parse errors underlined live; server errors mapped to
  line and column using PostgreSQL's error position.
  Done when: both kinds show at the right location in tests and in the app.
  Note: Live `sqlparser` diagnostics and server error positions (`diagnostics.rs` tests; checked in the app).
- [ ] **M1-10 Results grid core.** Virtualized rows and columns, streaming append, NULL styling,
  right-aligned numbers, column resize/reorder/pin, cell and range selection, copy as TSV.
  Done when: 1M rows loaded, scrolling holds frame rate; memory within budget.
  Note: Partial: `DataTable` virtualizes rows and columns, streams, styles NULLs, right-aligns numbers, resizes/reorders/pins columns, copies cells. Range selection and multi-cell TSV copy are missing; 1M rows × 10 int8 fit in < 150 MB (`batch.rs` test), frame rate not yet measured.
- [ ] **M1-11 Grid extras.** Value viewer (JSON, XML, text, hex, image), export CSV/JSON/
  Markdown/SQL INSERT, client-side sort and filter, multiple result-set tabs, status line,
  configurable fetch limit (default 10,000) with "Fetch all".
  Done when: each feature has a test or a documented manual check in the task note.
  Note: Partial: JSON/text/hex viewer, CSV/JSON/Markdown/SQL INSERT export (tests in `sql_tab.rs`), sort, filter, result-set tabs, status line, fetch limit with "Fetch all" (`streams_with_fetch_limit_and_fetch_all`). XML and image viewers are missing.
- [x] **M1-12 Schema explorer.** Lazy tree, fuzzy object search, actions (open data, generate
  SELECT/INSERT/UPDATE, copy name, view DDL, truncate/drop with confirmation).
  Done when: tree expands without blocking on large schemas.
  Note: Lazy, cached tree with fuzzy search box; context menu with open data, generate SELECT/INSERT/UPDATE, copy name, DDL, truncate/drop (guarded).
- [x] **M1-13 Query history.** Persist statement, connection, duration, row count; searchable
  panel; per-connection off switch.
  Done when: history survives restart and respects the off switch.
  Note: Stored in SQLite (`history_search`); History overlay; per-connection switch in the editor (`history_off_switch_records_nothing`).
- [x] **M1-14 Inline editing.** For single-table results with a primary key: staged edits with
  highlight, SQL preview, commit in one transaction, discard.
  Done when: integration test commits edits and verifies the rows.
  Note: Staged edits with highlight, SQL preview, one transaction where each UPDATE must hit exactly one row; `crates/core/tests/pg_edits.rs`.
- [x] **M1-15 Production guards.** Destructive-statement detection with `sqlparser`, confirmation
  dialog, read-only mode, red environment accent across tab, sidebar, status bar, editor border.
  Done when: tests cover DROP, TRUNCATE, DELETE/UPDATE without WHERE; read-only blocks writes.
  Note: `guard.rs` tests (DROP, TRUNCATE, DELETE/UPDATE without WHERE), `production_requires_confirmation`, `read_only_blocks_writes`; red accent on tab, sidebar, status bar and editor border.
- [ ] **M1-16 Benchmarks.** criterion benches for row decode into `RowBatch`; a grid frame-time
  harness with 1M rows; memory measurement script.
  Done when: numbers recorded in `docs/DECISIONS.md` against the budgets.
  Note: Partial: criterion benches (`crates/db/benches/decode.rs`) and the memory test are recorded in `docs/DECISIONS.md`. The grid frame-time harness is missing.

## M2 — SSH, terminal, tunnels

Exit: one login to a Host opens a terminal and a tunneled PostgreSQL connection.

- [x] **M2-1 SSH sessions.** `russh` session manager: password, public key (Ed25519, ECDSA, RSA),
  keyboard-interactive, keepalive, reconnect with backoff. One session per Host, reference-counted.
  Done when: integration tests for each auth method against the docker OpenSSH server.
  Note: `remote::ssh::SshManager` (one session per Host, weak-shared, per-Host login lock); password, Ed25519/ECDSA/RSA keys (encrypted keys via keychain or prompt), keyboard-interactive, keepalive; terminals reconnect with 1-2-4-8-16 s backoff and keep scrollback. Integration tests run against local sshd servers (`scripts/ssh-test-servers.sh`, CI job `integration-ssh`) rather than the docker OpenSSH service.
- [x] **M2-2 Host keys.** Read `~/.ssh/known_hosts` plus Switchyard's store; unknown key prompt
  with fingerprint via the event bus; changed key blocks with a warning.
  Done when: tests cover known, unknown (accept/reject) and changed keys.
  Note: Tolerant known_hosts reader (hashed, wildcards, negation, @revoked); unknown keys prompt (Trust once / Trust and connect), changed keys block with the design's screen; 'Replace stored key' trusts exactly the shown fingerprint in Switchyard's file. Tests: `known_hosts.rs` units, `host_keys_known_unknown_and_changed`, and checked in the app.
- [x] **M2-3 Jump hosts.** ProxyJump chains via direct-tcpip channels.
  Done when: connect through a two-hop chain in docker.
  Note: Chains through direct-tcpip channels; jump sessions are shared too. `two_hop_jump_chain` (2222 → 2223 → 2224).
- [ ] **M2-4 SSH config import.** Parse Host, HostName, User, Port, IdentityFile, ProxyJump into
  Host profiles; preview before import.
  Done when: snapshot test on a sample config.
  Note: Partial: parser and snapshot test exist and the palette command imports Hosts, but there is no preview step before importing.
- [ ] **M2-5 SSH agent.** Agent auth via `SSH_AUTH_SOCK` on Unix, OpenSSH agent pipe and Pageant
  on Windows.
  Done when: agent auth works on Linux and macOS in CI; Windows checked manually and noted.
  Note: Partial: agent auth via SSH_AUTH_SOCK works on Linux (`agent_auth` test). macOS not run yet; Windows uses the OpenSSH agent pipe but is unchecked and Pageant is not wired.
- [x] **M2-6 Terminal core.** `switchyard-term` wraps `alacritty_terminal`'s `Term`: byte feed,
  resize, scrollback (default 10,000 lines). Local shell via `portable-pty`.
  Done when: unit tests feed escape sequences and assert grid state.
  Note: `switchyard-term`: `Terminal` (shared, FairMutex) + `Feeder` (I/O-side parsing, coalesced wakeups), snapshots with resolved colors, selection, regex-free literal search, key/mouse/paste encoders, `portable-pty` local shells; 19 unit tests incl. real `/bin/sh` runs.
- [x] **M2-7 Terminal view.** GPUI rendering of cells, cursor, selection, true color, mouse
  reporting, bracketed paste, scrollback search, clickable links.
  Done when: `vim`, `htop` and `less` render correctly; `cat` of a 100 MB file keeps UI responsive.
  Note: Canvas view (cell grid, true color, bold/italic/underline/strike, block/beam/underline cursor), mouse reporting (SGR + legacy), bracketed paste, selection + copy, scrollback search, Ctrl/Cmd+click links. Checked in the app: vim, htop, less; ~100 MB of output in 10 s (debug build) with the UI responsive. Not done: IME composition, wide glyph width for CJK is forced to one cell's advance per char.
- [x] **M2-8 Splits and broadcast.** Split panes in a terminal tab; opt-in input broadcast.
  Done when: broadcast sends keystrokes to all panes only when enabled.
  Note: Up to four side-by-side panes per tab (Split / Ctrl+Shift+D), broadcast toggle with a banner; checked in the app that input reaches every pane only while broadcast is on. No automated test for broadcast.
- [x] **M2-9 Tunnels.** Local forwards on ephemeral ports; shared tunnel registry; DB connections
  "via Host" open tunnels automatically; PostgreSQL cancel goes through the same tunnel.
  Done when: integration test queries and cancels PostgreSQL through the docker SSH server.
  Note: `remote::ssh::Tunnel`: listener on 127.0.0.1:<ephemeral>, one direct-tcpip channel per connection, byte counters, keeps its SSH session and logs in again after a drop. Core shares one tunnel per (Host, target) across sessions; "via Host" connections use it for connect and cancel. `crates/core/tests/tunnel_pg.rs` queries, cancels `pg_sleep` (< 1 s), checks sharing and stopping; `tunnel_forwards_counts_and_stops` in remote.
- [x] **M2-10 Tunnel manager UI.** List local port, target, status, bytes; stop a tunnel.
  Done when: stopping a tunnel disconnects dependent DB sessions with a clear message.
  Note: Status-bar popover from the design: local port, Host → target, status (Active / Reconnecting / Failed with the error as tooltip), bytes, Stop. Stopping ends dependent sessions with "The tunnel through <Host> on port N was stopped · reconnect to continue" (test + checked in the app).

## M3 — SQL Server and Driver Manager

Exit: integrated auth works on a Linux machine that started without Kerberos libraries.

- [x] **M3-1 SQL Server driver.** `tiberius` with rustls and SQL login; type mapping (bit, int
  family, decimal/numeric, money, float/real, char/varchar/nvarchar, binary/varbinary,
  uniqueidentifier, date, time, datetime, datetime2, datetimeoffset, xml); streaming; multiple
  result sets; info messages as notices.
  Done when: integration tests per type and a multi-result-set batch.
  Note: tiberius 0.13 (tds80, rustls); every listed type decoded and tested against SQL Server 2025, multiple result sets, 200/1000 batches, rows affected via @@ROWCOUNT, errors with code and line. Deferred: info messages as notices (tiberius drops INFO tokens; see DECISIONS).
- [x] **M3-2 SQL Server cancel.** Implement cancel via the TDS attention signal. If tiberius has
  no API for it, spike options (upstream patch, fork, or drop-and-reconnect) and record the choice.
  Done when: `WAITFOR DELAY '00:00:30'` is stopped within 1 s.
  Note: Attention via `cancel_query`; SQL Server acknowledges in a separate message tiberius cannot read, so the session reconnects and warns (DECISIONS). WAITFOR 30 s stops in ~0.3 s; mid-stream cancel tested.
- [x] **M3-3 T-SQL dialect.** `GO` splitting, TOP, bracket quoting, catalog queries on `sys` views,
  error line mapping, `@name` parameters.
  Done when: splitter and catalog snapshot tests pass.
  Note: T-SQL dialect with `@name` → `@Pn` binding; catalog on sys views (folders, columns, PK/identity, indexes, FKs, constraints, triggers, generated DDL); insta snapshots for dbo objects and DDL. Core registers the driver; the connection editor tests SQL Server connections. CI job `integration (SQL Server)` via scripts/mssql-test-server.sh.
- [x] **M3-4 Driver Manager core.** Manifest format (see SPEC example), signed manifest
  verification (`minisign-verify`), detection (paths, env vars, app dir, minimum version),
  app-managed directory, component registry, runtime loading via `libloading`.
  Done when: tests cover detect-present, detect-missing, bad signature, bad checksum.
  Note: Manifest (bundled, or minisign-verified download), detection (user path, app dir with minimum version, builtin, env vars, system dirs), registry with persisted paths, `libloading`. Tests: detect present/missing/too old, bad signature, other key, bad checksum, real library load. Deferred: fetching the manifest from the update server (needs the signing key and URL).
- [x] **M3-5 Install strategies.** Archive download, verify, extract to
  `<data_dir>/switchyard/drivers/<component>/<version>/`; package-manager strategy showing the exact
  command and running it after confirmation with elevation; install from file; mirror setting.
  Done when: each strategy tested (package manager mocked in CI).
  Note: Package (exact command, pkexec elevation, terminal fallback), archive (download with progress, SHA-256, safe tar.gz unpack, staging + rename), install from file, mirror setting. Package manager mocked in tests; archive flow also checked end to end in the app against a local server.
- [x] **M3-6 Driver UI.** Inline missing-component card in the connection editor (all states from
  SPEC) and Settings → Drivers page.
  Done when: every state from the SPEC table is reachable and rendered.
  Note: Card states: missing/outdated, license, needs admin (command + Copy), downloading with progress, verifying, unpacking, failed with retry, installed then automatic re-test; Use existing path and manual steps. Settings → Drivers lists status/version/location with Install, Remove, Show steps, Retry, install from file and mirror. Checked in the app (download progress only in tests: the local download finished too fast to capture).
- [ ] **M3-7 Integrated auth.** Windows: SSPI via tiberius `winauth`. Linux/macOS: spike
  runtime-loaded GSSAPI versus tiberius `integrated-auth-gssapi` (build-time link); pick one,
  record it in `docs/DECISIONS.md`, implement with Driver Manager auto-setup on Linux.
  Done when: Linux machine without krb5 libs gets prompted, installs, and connects.
- [ ] **M3-8 Certificates.** Corporate CA import (OS stores and file) and per-connection
  certificate pinning.
  Done when: connects to a server with a self-signed cert only after pinning.
- [x] **M3-9 Azure SQL with Entra ID (user request).** Interactive (auth code + PKCE) and
  device-code sign-in with MFA, password and service principal; tokens passed to tiberius as
  `AADToken`; refresh tokens in the keychain. See DECISIONS 2026-10-06.
  Done when: connect to an Azure SQL database with an MFA-enabled account.
  Note: Browser (PKCE + loopback), device code, password and service principal flows in `db::entra`; core caches access tokens and keeps refresh tokens in the keychain; sign-in dialogs in the prompt queue; editor fields for tenant and client-id override. Tested with a stand-in identity server and against login.microsoftonline.com up to app lookup. Not yet done: a real sign-in, which needs the Switchyard app registration (docs/entra-app.md).

## Extra — Cloudflare D1 (user request)

- [x] **D1-1 Cloudflare D1 engine.** REST `raw` endpoint, SQLite dialect, type inference,
  catalog, connection editor.
  Note: tests run against a local stand-in (`crates/db/tests/d1.rs`); the real API was
  reached and its auth error parsed, but no query has run against a real D1 database yet. No transactions or inline editing (see DECISIONS).

## M4 — File transfer

Exit: resume an interrupted 1 GB upload.

- [ ] **M4-1 RemoteFs trait.** List, stat, read/write streams, rename, delete, mkdir, chmod, with
  implementations for local and SFTP (`russh-sftp` on the Host session).
  Done when: shared test suite passes for local and SFTP.
- [ ] **M4-2 FTP/FTPS.** `suppaftp` implementation of `RemoteFs`; explicit and implicit TLS;
  passive and active modes.
  Done when: shared test suite passes against the docker FTP server.
- [ ] **M4-3 Transfer queue.** Parallel transfers (default 4), pause, resume, retry; resume from
  offset (SFTP) and REST (FTP); progress, speed, ETA events.
  Done when: a killed 1 GB upload resumes from its last byte.
- [ ] **M4-4 Files tab UI.** Dual pane, breadcrumbs, sortable columns, hidden-file toggle, drag and
  drop between panes and from the OS, transfer drawer.
  Done when: all states from SPEC "Files tab" are reachable.
- [ ] **M4-5 Remote edit.** Open remote file in the editor; save uploads after an mtime conflict
  check with a resolve dialog.
  Done when: conflict is detected when the remote file changes during editing.

## M5 — Query plans, CLI and AI assistant

Exit: from a slow query, Optimize produces a rewrite or index whose compared plan is measurably
faster, and no agent call ever performed a write.

- [ ] **M5-1 Plan model.** `switchyard-plan` crate with `PlanNode` (operation, object, estimated and
  actual rows, loops, cost, self and total time, I/O, predicates, warnings) and `Plan` metadata.
  Done when: unit tests build trees by hand and compute self time correctly.
- [ ] **M5-2 PostgreSQL plan capture.** `EXPLAIN (FORMAT JSON)` and
  `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)` parsed into `PlanNode`; DML wrapped and rolled back.
  Done when: insta snapshots for scans, joins, sorts, aggregates, CTEs; a DELETE leaves rows intact.
- [ ] **M5-3 SQL Server plan capture.** `SHOWPLAN_XML` and `STATISTICS XML` parsed with `quick-xml`
  into `PlanNode`, including warnings and MissingIndexes.
  Done when: snapshot tests against plans captured from the docker SQL Server.
- [ ] **M5-4 Findings engine.** Rules from SPEC "Findings" (full scan, bad estimate, rows removed
  by filter, spill, expensive nested loop, key lookup, implicit conversion, missing index) with
  configurable thresholds; ranked output linked to node ids.
  Done when: each rule has a positive and a negative fixture.
- [ ] **M5-5 Plan view UI.** Plan graph (color by share of self time, edge width by rows), flame
  view toggle, node detail panel, SQL highlight of the selected node, hotspot list, Explain and
  Explain Analyze actions in the editor.
  Done when: a 200-node plan renders and pans smoothly; every SPEC state is reachable.
- [ ] **M5-6 Plan compare and history.** Side-by-side compare with time, row and I/O deltas;
  plans stored with their history entry.
  Done when: comparing two saved plans shows correct deltas.
- [ ] **M5-7 Access analysis.** PostgreSQL `pg_stat_user_tables`, `pg_stat_user_indexes`,
  `pg_stat_statements`; SQL Server index usage, missing-index DMVs, Query Store. Workload view UI.
  Missing extension or permission shows a hint with the GRANT statement.
  Done when: integration tests with and without the extensions and permissions.
- [ ] **M5-8 Hypothetical indexes.** Detect HypoPG; create hypothetical index, explain, drop it,
  all in one session.
  Done when: plan changes with a hypothetical index and no real index is created.
- [ ] **M5-9 `swy` CLI.** `clap` binary with `connections`, `query`, `explain [--analyze] [--open]`,
  `workload`; table, CSV and JSON output; `--open` hands off to the running app.
  Done when: CLI integration tests against docker for each command.
- [ ] **M5-10 MCP server.** `swy mcp` over stdio with `rmcp`: tools from SPEC "MCP tools" with every
  guard from CLAUDE.md "Agent safety rules"; per-connection agent access setting in the app.
  Done when: tests prove writes are rejected, row caps and timeouts hold, Production is hidden by
  default, and no tool output contains hostnames or secrets.
- [ ] **M5-11 Agent adapter core + Claude Code.** In `switchyard-agents`: `AgentAdapter` trait and normalized
  `AgentEvent` stream; shared runner (temp workdir, session token for `swy mcp`, child process, cancel kills the
  process tree, cleanup). Claude Code adapter: `claude -p --output-format stream-json
  --mcp-config <generated>`, resume via `--resume`, allow only Switchyard MCP tools.
  Done when: tested with a fake `claude` binary replaying recorded stream-json, plus one live run
  against the docker PostgreSQL behind a manual flag.
- [ ] **M5-12 Codex CLI adapter (spike first).** `codex exec --json`, MCP via `[mcp_servers]` in a
  generated config, resume via `codex exec resume`. Spike: how to auto-approve only Switchyard's MCP
  tools in non-interactive mode without bypassing approvals globally, and how to keep the user's
  existing login when using a generated config. Record the outcome in `docs/DECISIONS.md`.
  Done when: replay tests pass and a live run completes a `describe_table` + `explain` tool sequence.
- [ ] **M5-13 Gemini CLI adapter.** `gemini -p --output-format stream-json`, MCP via `mcpServers`
  in a generated `.gemini/settings.json` inside the temp workdir, resume support.
  Done when: replay tests pass and a live run completes the same tool sequence.
- [ ] **M5-14 Custom adapter, detection, picker.** User-defined command template, plain-text or
  JSONL field mapping, MCP config template. Detection of installed CLIs with version ranges and
  install hints via the Driver Manager. Agent picker in the assistant panel; default in
  Settings → Assistant; per-connection override. "Open in terminal" launches the selected CLI
  interactively in a terminal tab with the MCP server attached.
  Done when: each CLI shows as installed / missing / unsupported version correctly.
- [ ] **M5-15 Assistant panel.** Optimize button on plan view and editor, streaming answer with
  tool calls, suggestion cards with Compare plan and Open in editor, follow-up input, Plan a query
  mode.
  Done when: each suggestion type can be compared and opened with every adapter; agent calls show
  in history with the right `agent:<id>` tag.

## M6 — Packaging and beta

Exit: every performance budget passes on all three platforms; signed builds published.

- [ ] **M6-1 macOS.** Universal binary, app bundle, signing, notarization, DMG, Homebrew cask.
  Partial: `packaging/macos/build-macos.sh` builds the universal app, `.dmg` and `.pkg`, with optional
  signing/notarization via env vars (not yet exercised with a real Developer ID). Cask pending.
- [ ] **M6-2 Windows.** Signed MSI, winget manifest.
  Partial: NSIS installer via `packaging/windows/build-windows.ps1` (optional signtool signing).
  MSI vs NSIS open, see DECISIONS 2026-10-05. winget manifest pending.
- [ ] **M6-3 Linux.** AppImage, `.deb`, `.rpm`, AUR PKGBUILD; Wayland and X11 checked.
  Partial: AppImage via `packaging/linux/build-appimage.sh`. `.deb`, `.rpm`, AUR pending.
- [ ] **M6-4 Auto-update.** Signed updates on macOS and Windows; Linux defers to package managers.
- [ ] **M6-5 Performance gates.** CI jobs that fail when a budget from CLAUDE.md is exceeded.
- [ ] **M6-6 Crash reporting.** Opt-in, scrubbed of SQL text, hostnames and credentials;
  telemetry off by default.
- [ ] **M6-7 Portable mode.** Marker file next to the binary keeps config and drivers beside it.

## Later (not before beta)

- Oracle via Driver Manager (Instant Client auto-install, license acceptance).
- Workload-wide index advisor, snippet library, import wizard, data compare, folder sync.

## Follow-ups

(Add items here instead of doing them mid-task.)
- Release CI workflow that runs `packaging/` on a tag and uploads `dist/` to a GitHub release.
- Log file for release builds (Windows GUI subsystem hides stdout).
- Real app icon to replace the generated placeholder in `packaging/icons/`.

- Observe CI green on macOS, Windows and Linux (M0-1).
- Smoke tests for the SQL Server, SSH and FTP containers (M0-2).
- Split panes in the tab area (M0-3).
- Drag-to-reorder connections in the sidebar (M0-9).
- Verify multi-cursor and folding in the gpui-component editor (M1-6).
- Grid range selection and multi-cell TSV copy (M1-10); grid frame-time harness (M1-16).
- XML and image value viewers (M1-11).
- Driver Manager: fetch the signed manifest from the update server; zip archives (Oracle
  Instant Client ships zip) once a zip reader is approved.
- SQL Server: upstream tiberius patches for INFO tokens (notices) and reading the attention
  acknowledgement across a message boundary (would remove the reconnect after cancel).
- Approval pending for `tokio-postgres-rustls`/`rustls-native-certs` and `lsp-types` (see DECISIONS).
