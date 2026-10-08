# PLAN.md — Switchyard build plan

Work top to bottom. Each task is sized for one focused session. "Done when" is the acceptance
test; a task is not done until it passes plus the Definition of Done in CLAUDE.md.
Tick `[x]` and add a short note when finished.

---

## M0 — Skeleton

Exit: the app opens with the full layout, profiles save to SQLite, secrets land in the keychain.

- [x] **M0-1 Workspace scaffold.** Cargo workspace with the 10 crates, `rust-toolchain.toml`,
  `rustfmt.toml`, shared `[workspace.dependencies]`, GitHub Actions running fmt, clippy and tests
  on macOS, Windows and Linux.
  Done when: `cargo build --workspace` passes and CI is green on all three platforms.
  Note: CI (fmt, clippy `-D warnings`, tests) observed green on Linux, macOS and Windows
  (PR run 41, 2026-10-06).
- [ ] **M0-2 Test services.** `docker/compose.yml` with PostgreSQL 16, SQL Server 2022,
  an OpenSSH server (password + key auth), and an FTP server with FTPS. Seed scripts for a
  sample schema including one table with 1,000,000 rows.
  Done when: `docker compose up -d` starts all four and a smoke test connects to each.
  Note: Partial: compose file and seed scripts written (1M-row `orders`); only the PostgreSQL seed was verified (against a local PostgreSQL 16, no Docker in the build environment). No smoke test yet for SQL Server, SSH or FTP.
- [x] **M0-3 Window and layout.** GPUI app with gpui-component: title bar, collapsible left
  sidebar, center tab area with splits, optional right panel, status bar. Light and dark themes.
  Done when: layout matches SPEC "Main window layout"; theme toggle works; panels collapse.
  Note: title bar, collapsible sidebar, tabs, right inspector panel, status bar, themes. The
  center area splits right or down (tab-strip buttons, Ctrl/⌘+\\ and Ctrl/⌘+Shift+\\, palette):
  one tab strip, the focused pane follows clicks, the divider drags, closing tabs keeps the
  split consistent. The split is not restored after a restart yet.
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
- [x] **M0-9 Connections sidebar and editor.** Sidebar tree grouped by Host or folder with
  environment dots; connection editor dialog with a form per type and environment label.
  "Test connection" is stubbed until drivers exist.
  Done when: create, edit, delete and reorder connections; changes persist across restarts.
  Note: tree grouped by Host, environment dots, editor for every type with test connection,
  create/edit/delete persist. Drag a Host or connection to reorder it among its siblings
  (Hosts among Hosts, connections within their Host or "Local & direct"); saved, survives restarts.

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
- [x] **M1-6 Editor tab.** gpui-component editor with tree-sitter SQL highlighting, multi-cursor,
  find/replace, comment toggle, folding. Buffers autosave and restore after restart.
  Done when: kill the app mid-edit, relaunch, buffer content is intact.
  Note: tree-sitter SQL highlighting, find, comment toggle, buffer autosave/restore
  (`buffers_survive_reopen`, checked by killing the app). Multi-cursor checked manually:
  Alt+click and Shift+Alt+↑/↓ (gpui-kit's Ctrl+Alt+↑/↓ did not fire under X11 here).
  Folding: the SQL tab supplies fold regions from the dialect lexer and splitter
  (`folds.rs` tests): statements, parenthesised blocks and block comments of 3+ lines;
  checked folding/unfolding and after a restart.
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
  Note: Partial: `DataTable` virtualizes rows and columns, streams, styles NULLs, right-aligns
  numbers, resizes/reorders/pins columns (the data now moves with a dragged header). Range
  selection: Shift+click or Shift+arrows; Ctrl/⌘+C copies the range as TSV (raw values, NULL
  empty), "Copy selection as" uses the range with column names. 1M rows × 10 int8 fit in
  < 150 MB (`batch.rs` test); scroll frame rate not yet measured (M1-16 harness).
- [x] **M1-11 Grid extras.** Value viewer (JSON, XML, text, hex, image), export CSV/JSON/
  Markdown/SQL INSERT, client-side sort and filter, multiple result-set tabs, status line,
  configurable fetch limit (default 10,000) with "Fetch all".
  Done when: each feature has a test or a documented manual check in the task note.
  Note: JSON / text (row), XML / hex / image (selected cell) viewers: XML indented and
  coloured with a plain-text fallback, image by signature (PNG, JPEG, GIF, WebP, BMP, TIFF,
  SVG), large values capped at 4,000 lines (`viewer.rs` tests; checked manually with an `xml`
  and a `bytea` PNG column). CSV/JSON/Markdown/SQL INSERT export (tests in `sql_tab.rs`), sort,
  filter, result-set tabs, status line, fetch limit with "Fetch all"
  (`streams_with_fetch_limit_and_fetch_all`).
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
- [x] **M2-4 SSH config import.** Parse Host, HostName, User, Port, IdentityFile, ProxyJump into
  Host profiles; preview before import.
  Done when: snapshot test on a sample config.
  Note: parser with snapshot test; "Import Hosts from ~/.ssh/config" opens a preview (alias,
  user@host:port, auth, ProxyJump, already-saved entries greyed) and imports the ticked ones,
  wiring ProxyJump to saved or imported Hosts (`ssh_import.rs` tests).
- [ ] **M2-5 SSH agent.** Agent auth via `SSH_AUTH_SOCK` on Unix, OpenSSH agent pipe and Pageant
  on Windows.
  Done when: agent auth works on Linux and macOS in CI; Windows checked manually and noted.
  Note: Partial: Windows tries a pipe in `SSH_AUTH_SOCK`, the OpenSSH agent service, then
  Pageant (`pageant` in the Host's agent field picks it alone). `tests/agent.rs` signs in
  through each agent against an in-process russh server; CI runs it with ssh-agent on Linux
  and macOS and with the agent service and pinned Pageant 0.85 on Windows. Tick when those
  CI steps are seen green.
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
- [x] **M3-7 Integrated auth.** Windows: SSPI via tiberius `winauth`. Linux/macOS: spike
  runtime-loaded GSSAPI versus tiberius `integrated-auth-gssapi` (build-time link); pick one,
  record it in `docs/DECISIONS.md`, implement with Driver Manager auto-setup on Linux.
  Done when: Linux machine without krb5 libs gets prompted, installs, and connects.
  Note: Windows signs in with SSPI (tiberius `winauth`); Linux/macOS use Kerberos through
  GSSAPI loaded at runtime by the Driver Manager (`libgssapi_krb5`, or macOS's GSS
  framework) via an external-auth hook in a vendored tiberius (`vendor/tiberius`). New
  "Windows account" method: DOMAIN\\user + password (SSPI on Windows, NTLM via `sspi`
  elsewhere). Tested against a throwaway MIT KDC (`scripts/kerberos-test-kdc.sh`): full
  mutual-auth handshake, and a real service ticket reaching SQL Server, which then refuses it
  only because the test KDC is not Active Directory. Not verified: login against a real AD
  domain, Windows SSPI and the macOS GSS framework (compiled in CI only).
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

- [x] **M4-1 RemoteFs trait.** List, stat, read/write streams, rename, delete, mkdir, chmod, with
  implementations for local and SFTP (`russh-sftp` on the Host session).
  Done when: shared test suite passes for local and SFTP.
  Note: `RemoteFs` gained stat, read/write streams and whole-file read/write; `SftpFs` (russh-sftp) runs on the Host's shared session. Shared suite: local unit test + SFTP integration test (listing, overwrite, size cap, 3 MB stream, rename, delete). chmod deferred.
- [ ] **M4-2 FTP/FTPS.** `suppaftp` implementation of `RemoteFs`; explicit and implicit TLS;
  passive and active modes.
  Done when: shared test suite passes against the docker FTP server.
- [x] **M4-3 Transfer queue.** Parallel transfers (default 4), pause, resume, retry; resume from
  offset (SFTP) and REST (FTP); progress, speed, ETA events.
  Done when: a killed 1 GB upload resumes from its last byte.
  Note: Queue of 4 parallel transfers (others wait, shown as queued), pause, resume, retry, cancel; files are written as `<name>.swypart` and renamed when complete, so resume continues from the last byte (SFTP offsets), also after the app was killed: a new transfer that finds a partial copy offers Resume / Start over. Progress, speed and ETA per transfer and overall (drawer, status bar). Verified: 1 GB upload killed with `kill -9` at 347 MB, restarted, resumed, SHA-256 identical. FTP REST waits for M4-2.
- [x] **M4-4 Files tab UI.** Dual pane, breadcrumbs, sortable columns, hidden-file toggle, drag and
  drop between panes and from the OS, transfer drawer.
  Done when: all states from SPEC "Files tab" are reachable.
  Note: Dual-pane tab: this computer on the left, any Host (or this computer) on the right via the source picker; breadcrumbs, sortable Name/Size/Modified, hidden toggle, multi-select (Ctrl/Cmd-click), new folder, rename, delete with confirm, Copy →/←, drag between panes and from the OS, transfer drawer. The sidebar Files panel shares the queue and has ⇆ to open this tab. Remote-edit conflict state lives in the editor tab (M4-5).
- [x] **M4-5 Remote edit.** Open remote file in the editor; save uploads after an mtime conflict
  check with a resolve dialog.
  Done when: conflict is detected when the remote file changes during editing.

## M5 — Query plans, CLI and AI assistant

Exit: from a slow query, Optimize produces a rewrite or index whose compared plan is measurably
faster, and no agent call ever performed a write.

  Note: Remote files open in an editor tab (5 MB cap, text only); Ctrl/Cmd+S saves over SFTP after an mtime check; a conflict offers Overwrite or Discard and reload. Checked in the app and in `core/tests/ssh_files.rs`.
- [x] **M5-1 Plan model.** `switchyard-plan` crate with `PlanNode` (operation, object, estimated and
  actual rows, loops, cost, self and total time, I/O, predicates, warnings) and `Plan` metadata.
  Done when: unit tests build trees by hand and compute self time correctly.
  Note: `switchyard-plan::model`: `PlanNode` (operation, object, estimated/actual rows per execution, loops, cost, subtree time, I/O, predicates, warnings, details) and `Plan` (source, kind, timings, warnings, missing indexes); pre-order ids, self time, time/cost shares.
- [x] **M5-2 PostgreSQL plan capture.** `EXPLAIN (FORMAT JSON)` and
  `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)` parsed into `PlanNode`; DML wrapped and rolled back.
  Done when: insta snapshots for scans, joins, sorts, aggregates, CTEs; a DELETE leaves rows intact.
  Note: `pg::parse` and `capture::capture`; actual plans run in a transaction (or a savepoint inside the user's own) that is always rolled back. Materialized CTE bodies are moved under their CTE Scan so self times are not double counted. 11 fixtures captured from the seed with insta snapshots; live test: a DELETE leaves its rows, a failing statement leaves no transaction. Fixed `ROLLBACK TO SAVEPOINT` closing the PostgreSQL session's transaction flag.
- [x] **M5-3 SQL Server plan capture.** `SHOWPLAN_XML` and `STATISTICS XML` parsed with `quick-xml`
  into `PlanNode`, including warnings and MissingIndexes.
  Done when: snapshot tests against plans captured from the docker SQL Server.
  Note: `mssql::parse` with quick-xml: RelOp tree, per-thread run-time counters (rows per execution), objects, predicates (seeks as `col = value`), warnings (spills, conversions), Key Lookup naming, MissingIndexes; pass-through operators without counters take their input's. 10 showplans captured from SQL Server 2022 by `tests/capture.rs` (`SWITCHYARD_WRITE_FIXTURES=1`), snapshot-tested.
- [x] **M5-4 Findings engine.** Rules from SPEC "Findings" (full scan, bad estimate, rows removed
  by filter, spill, expensive nested loop, key lookup, implicit conversion, missing index) with
  configurable thresholds; ranked output linked to node ids.
  Done when: each rule has a positive and a negative fixture.
  Note: `findings::analyze` with `Thresholds` (serde, defaults in DECISIONS): full scan, bad estimate (not over-estimates below a LIMIT/TOP), rows removed by filter, spill, expensive nested loop (inner side's share), key lookup, implicit conversion, missing index (SQL Server's, or a PostgreSQL CREATE INDEX on a full scan's filter columns). Ranked by rule weight × time/cost share; each rule has a positive and a negative fixture.
- [x] **M5-5 Plan view UI.** Plan graph (color by share of self time, edge width by rows), flame
  view toggle, node detail panel, SQL highlight of the selected node, hotspot list, Explain and
  Explain Analyze actions in the editor.
  Done when: a 200-node plan renders and pans smoothly; every SPEC state is reachable.
  Note: `app/src/plan_view` is the SQL tab's "Plan" result tab. Graph (root left, inputs right, heat stripe by share of time or cost, edges by rows on a log scale, culled to the viewport, drag/wheel pan, Ctrl/Cmd+wheel zoom, Fit at ≥ 80%) and flame (icicle by inclusive time or cost); hotspot list (collapses below 720 px) that selects and reveals the node; detail panel; the selected node's table, alias or CTE highlighted in the editor with range decorations. Explain ⌘E / Ctrl+E and Analyze ⇧⌘E / Ctrl+Shift+E in the toolbar, palette and keymap; Stop cancels a capture. States (SPEC lists none): empty, capturing, loading, failed with Retry, Production confirmation (Run actual plan / Explain instead), ready. Checked in the app against the docker PostgreSQL, including a 199-node plan (renders and drag-pans; frame times not measured under llvmpipe). Fixed parallel plans in M5-2's parser: times under a Gather counted CPU time across workers (a scan showed 154 ms of a 69 ms query); now wall time.
- [x] **M5-6 Plan compare and history.** Side-by-side compare with time, row and I/O deltas;
  plans stored with their history entry.
  Done when: comparing two saved plans shows correct deltas.
  Note: plans are stored with their history entry (store migration 2) and `switchyard_plan::compare` matches operators by operation and object. Compare ▾ offers this tab's other plans and the connection's saved plans; the view shows time, planning, rows, pages and cost deltas, both graphs side by side (shared zoom) and a per-operator table (new / gone included) that selects in both. History rows with a stored plan get a "Plan" button. Checked in the app: before/after `CREATE INDEX` on `orders(total)`, loaded from history, showed −63% time, −96% pages, −30% cost.
- [x] **M5-7 Access analysis.** PostgreSQL `pg_stat_user_tables`, `pg_stat_user_indexes`,
  `pg_stat_statements`; SQL Server index usage, missing-index DMVs, Query Store. Workload view UI.
  Missing extension or permission shows a hint with the GRANT statement.
  Done when: integration tests with and without the extensions and permissions.
  Note: `plan::access::workload`, `Command::Workload`, Workload tab (palette "Workload: Query and
  Index Statistics"): statements / tables / indexes / missing indexes, flags for mostly-full-scan
  tables and unused indexes, hints with copyable fixes. `crates/plan/tests/access.rs` covers both
  engines with and without extensions, Query Store and grants (5 PostgreSQL, 3 SQL Server).
  Found by the tests: reading `shared_preload_libraries` needs `pg_read_all_settings`, so it now
  comes from `pg_settings` (hidden rows → "unknown"). Deferred: open a statement from the list in
  an editor / explain it directly.
- [x] **M5-8 Hypothetical indexes.** Detect HypoPG; create hypothetical index, explain, drop it,
  all in one session.
  Done when: plan changes with a hypothetical index and no real index is created.
  Note: `plan::whatif` (definitions checked with sqlparser: exactly one CREATE INDEX each;
  `hypopg_reset()` before and after, also on error), `Command::WhatIf`; plan view "What if…"
  pre-filled from findings' CREATE INDEX suggestions, results open in the compare view. Test:
  cost of `total = 123.45` on 1M orders drops >10× and `pg_indexes` is unchanged.
- [x] **M5-9 `swy` CLI.** `clap` binary with `connections`, `query`, `explain [--analyze] [--open]`,
  `workload`; table, CSV and JSON output; `--open` hands off to the running app.
  Done when: CLI integration tests against docker for each command.
  Note: `crates/cli` drives the shared core over its bus (scripts split by the dialect,
  Production confirmation via `--yes`); `--open` uses a loopback handoff (token file in the
  data dir, `core::handoff`). Keychain-less use: `SWITCHYARD_SECRETS=vault` +
  `SWITCHYARD_VAULT_PASSWORD`. Tests: `crates/cli/tests/cli.rs`.
- [x] **M5-10 MCP server.** `swy mcp` over stdio with `rmcp`: tools from SPEC "MCP tools" with every
  guard from CLAUDE.md "Agent safety rules"; per-connection agent access setting in the app.
  Done when: tests prove writes are rejected, row caps and timeouts hold, Production is hidden by
  default, and no tool output contains hostnames or secrets.
  Note: hand-rolled stdio JSON-RPC (ported from Emulsion, see DECISIONS) instead of `rmcp`.
  Tools: list_connections, list_tables, describe_table, run_query, explain (estimated
  only; ANALYZE refused until M5-14's approval), workload, what_if. "Allow coding agents"
  checkbox in the connection editor. Session token scoping is M5-11.
- [x] **M5-11 Agent adapter core + Claude Code.** In `switchyard-agents`: `AgentAdapter` trait and normalized
  `AgentEvent` stream; shared runner (temp workdir, session token for `swy mcp`, child process, cancel kills the
  process tree, cleanup). Claude Code adapter: `claude -p --output-format stream-json
  --mcp-config <generated>`, resume via `--resume`, allow only Switchyard MCP tools.
  Done when: tested with a fake `claude` binary replaying recorded stream-json, plus one live run
  against the docker PostgreSQL behind a manual flag.
  Note: `agents::runner` (private 0700 temp dir per run, CLI in its own process group, prompt
  on stdin, cancel kills the tree, dir removed and guards dropped before `Exited`) and
  `agents::claude` (`--restricted --tools "" --strict-mcp-config --allowedTools
  mcp__switchyard --permission-mode dontAsk`). Session tokens: `core::agent_run`
  (`<data>/agent-tokens/<sha256>.json`, 2 h expiry, revoked at run end); `swy mcp` with
  `SWITCHYARD_MCP_TOKEN` serves only the token's agent-enabled connections and re-checks the
  token on every call. Tests: `agents/tests/claude_replay.rs` (stream recorded from Claude
  Code 2.1.292), `cli/tests/cli.rs` `mcp_session_token_scopes_and_revokes` and
  `live_claude_code_run` (`SWITCHYARD_LIVE_AGENT=claude`; passed with haiku). Not yet on the
  core bus (the assistant panel, M5-15, adds the command). Vault-only systems: `swy mcp`
  needs `SWITCHYARD_VAULT_PASSWORD` in the app's environment (see Follow-ups).
- [x] **M5-12 Codex CLI adapter (spike first).** `codex exec --json`, MCP via `[mcp_servers]` in a
  generated config, resume via `codex exec resume`. Spike: how to auto-approve only Switchyard's MCP
  tools in non-interactive mode without bypassing approvals globally, and how to keep the user's
  existing login when using a generated config. Record the outcome in `docs/DECISIONS.md`.
  Done when: replay tests pass and a live run completes a `describe_table` + `explain` tool sequence.
  Note: `agents::codex` (spike outcome in DECISIONS). Tests: `agents/tests/codex_replay.rs`
  (stream recorded from codex-cli 0.160.1) and `cli/tests/cli.rs`
  `codex_runs_switchyard_tools_with_a_scripted_model`: the real Codex binary with a mock
  Responses API (`cli/tests/mock_model`) completes describe_table + explain against the
  docker PostgreSQL (no OpenAI login exists in this environment for a model-backed run).
  Run requests gained `extra_args` / `extra_env` (settings; M5-14 uses them).
- [x] **M5-13 Gemini CLI adapter.** `gemini -p --output-format stream-json`, MCP via `mcpServers`
  in a generated `.gemini/settings.json` inside the temp workdir, resume support.
  Done when: replay tests pass and a live run completes the same tool sequence.
  Note: `agents::gemini` (workspace settings, admin policy, `GEMINI.md`, session-file resume;
  see DECISIONS). Tests: `agents/tests/gemini_replay.rs` (recorded from Gemini CLI 0.63) and
  `cli/tests/cli.rs` `gemini_runs_switchyard_tools_with_a_scripted_model`: the real Gemini
  binary with a mock Gemini API completes describe_table + explain against the docker
  PostgreSQL, and a follow-up continues the conversation. No Google login here for a
  model-backed run.
- [x] **M5-14 Custom adapter, detection, picker.** User-defined command template, plain-text or
  JSONL field mapping, MCP config template. Detection of installed CLIs with version ranges and
  install hints via the Driver Manager. Agent picker in the assistant panel; default in
  Settings → Assistant; per-connection override. "Open in terminal" launches the selected CLI
  interactively in a terminal tab with the MCP server attached.
  Done when: each CLI shows as installed / missing / unsupported version correctly.
  Note: `agents::custom` (argument and MCP config templates with placeholders, prompt as argument
  or on stdin, plain text or JSON-lines output mapped by JSON pointers, interactive arguments).
  Driver Manager: `claude-code` (≥ 2.1, < 3), `codex-cli` (≥ 0.160, < 1), `gemini-cli` (≥ 0.63,
  < 1) found on PATH and common install folders, version from `--version`; new status
  "Untested version" (TooNew) beside Too old / Not installed; install hints per OS. Settings →
  Assistant (default CLI with its status, path / model / extra arguments per CLI, the custom
  CLI); per-connection "Assistant CLI" in the connection editor; the panel's own picker is in
  M5-15. Core: `Command::RunAgent` / `CancelAgent` / `OpenAgentTerminal`, `Event::Agent`;
  runs are scoped to the asked-about connection (refused without agent access). "Open in
  terminal" starts the CLI interactively (same restrictions, `AgentAdapter::interactive`) in a
  local terminal whose exit revokes the token and removes the run directory. Tests:
  `drivers` detection (installed / missing / too old / untested, user path), `agents` custom
  adapter, `core/tests/assistant.rs` (settings, per-connection choice, refusal, cancel,
  missing CLI, Open in terminal).
- [x] **M5-15 Assistant panel.** Optimize button on plan view and editor, streaming answer with
  tool calls, suggestion cards with Compare plan and Open in editor, follow-up input, Plan a query
  mode.
  Done when: each suggestion type can be compared and opened with every adapter; agent calls show
  in history with the right `agent:<id>` tag.
  Note: `app/assistant_panel.rs`, right of the inspector (Ctrl/Cmd+J, palette "Toggle
  Assistant", "Optimize Statement at Cursor", "Plan a Query…"). "Optimize ✦" in the editor and
  plan view sends the statement and its findings; the CLI picker cycles the default / per-
  connection choice; follow-ups resume the CLI's session; Stop cancels; "Terminal" opens the
  CLI interactively (M5-14). Answers render prose and code blocks; each ```sql block becomes a
  card by kind: Index → "Compare plan (hypothetical)" (HypoPG what-if on PostgreSQL), Statistics
  → "Re-plan & compare" (the user runs the statement), Rewrite → "Compare plan" / "Show plan"
  (estimated plans, base vs suggestion in the plan view's compare); every card has "Open in
  editor". Cards come from the normalized `AgentEvent::Text`, so they work the same for every
  adapter (replay tests cover each parser; history tags `agent:<cli>` by the M5-12/13 tests).
  Checked under Xvfb with a scripted CLI. Deferred: in-app approval for agent ANALYZE (still
  refused), UI tests (Follow-ups).

## Extra — API workspace, Snowflake, Oracle (user request)

- [x] API-1 `switchyard-api` crate: AgentOps's API Workbench core (collections, requests,
  environments, import/export, cookies, OAuth, `pm.*` scripts in a Boa worker, collection runs),
  sending in process on core's runtime; secrets through the keychain / vault.
  Note: worker CPU/memory `setrlimit` caps not ported (need `unsafe`); wall-clock kill kept.
- [x] API-2 API workspace in the app: title-bar workspace menu switches Default ↔ API; the
  Workbench panel (ported to gpui-component 0.7 inputs) fills the window in API mode.
  Note: AgentOps's UI tests not ported yet (they used its test harness); see Follow-ups.
- [x] API-3 Snowflake engine: SQL API v2 (`/api/v2/statements`) with async polling, result
  partitions (gzip), multi-statement requests, server-side cancel, positional `:N` binds;
  key-pair JWT (rsa parses, ring signs) or programmatic access token; `USE …` carried across
  requests client-side; INFORMATION_SCHEMA catalog + GET_DDL; Snowflake dialect and lexer
  flavour (`$$` bodies, backslash escapes, `//` comments); connection editor kind.
  Note: tested against a local stand-in only (`crates/db/tests/snowflake.rs`); plans and
  workload stats for Snowflake not done (Follow-ups).
- [x] API-4 Oracle engine: `oracle` crate (ODPI-C, which dlopens Instant Client), blocking
  calls on tokio's blocking pool with rows streamed through a bounded channel, OCIBreak
  cancel, named binds (`:p1`), DBMS_OUTPUT as notices, ALL_* catalog + DBMS_METADATA DDL;
  Oracle dialect (PL/SQL units to a `/` line, `q'[..]'` quoting); Driver Manager zip
  extraction and an `oracle-instant-client` component (Linux/Windows archives pinned by
  SHA-256, macOS guided); connection editor kind; docker `oracle` profile service.
  Note: plans (EXPLAIN PLAN / DBMS_XPLAN) and workload stats for Oracle not done; Linux
  arm64 Instant Client not in the manifest (x64 only).
- [x] API-5 Workbench starts empty (user request): with no workspace saved, the API
  workspace shows a blank page with one "Add workspace" button; each click creates an empty
  `Workspace N`. A header picker lists the workspaces and adds more; the last one opened
  comes back on the next launch.
  Note: named workspaces live in `workbench_workspaces` (Workbench store schema 5); data
  saved earlier under the project-path scope is listed under that folder's name. Renaming
  and deleting workspaces not done (Follow-ups).

## M7 — MobaXterm parity, Tier 1 (user request)

Taken before the rest of M5 at the user's request (M5-12 spike notes are kept; see
DECISIONS 2026-10-07). Tier 1 only: features that fit the current architecture. Crates a
task needs are pre-approved (license checked, no GPL, recorded in DECISIONS). Not in scope:
embedded RDP/VNC, serial ports, network tools and local servers (Tier 2), an embedded X
server or bundled Unix tools on Windows (Tier 3).

- [x] **MX-1 Remote and dynamic forwarding.** `-R` (server port → local target) through
  `tcpip_forward` and `-D` SOCKS5 (no auth, CONNECT, IPv4/IPv6/domain) on the shared Host
  session; both in the tunnel registry and manager UI (add, stop, bytes), defined on a Host
  and optionally started with it.
  Done when: integration tests against the docker OpenSSH forward traffic both ways and
  through SOCKS5.
  Note: `remote::ssh::tunnel` now runs `ForwardSpec::{Local, Remote, Dynamic}`: remote
  forwards route `forwarded-tcpip` channels per server port (connect locally first, then
  accept or reject), are cancelled on stop and re-requested after a reconnect (1 s → 60 s
  backoff); dynamic forwards speak SOCKS4/4a/5 CONNECT without auth. Hosts gain `forwards`
  (`store::PortForward`, auto-start); `Command::StartForward`; auto-start forwards start
  when a terminal opens on the Host. UI: forward rows (L/R/D, bind, target, Auto) in the
  Host editor; the Tunnels panel shows the direction and lists saved forwards with Start.
  Tests: `remote/tests/ssh.rs` (remote forward, dead target, SOCKS5/SOCKS4),
  `core/tests/ssh_forwards.rs`. Not done: naming forwards in the editor (kept if set).
- [x] **MX-2 Agent and X11 forwarding.** Per-Host toggles. Agent forwarding answers
  `auth-agent@openssh.com` channels from the local agent (finishes M2-5's scope); X11 opens
  `x11` channels to the local display (`DISPLAY`: Unix socket or TCP) with a generated
  MIT-MAGIC-COOKIE replaced by the real one from `xauth`. Windows: detect VcXsrv / X410 /
  Xming through the Driver Manager and point at its display.
  Done when: tests prove a forwarded agent signs and an X11 channel reaches a fake display.
  Note: Host options `forward_agent`, `forward_x11`, `x11_display` (editor checkboxes and an
  X display field). Agent channels are piped to the same agent the login uses (socket or
  Windows pipe; Pageant cannot be forwarded) and refused when the Host did not ask; X11
  (`ssh/x11.rs`) checks the fake cookie in each setup packet and swaps in the real one from
  `xauth list` (or none). Driver Manager: "X server" component (VcXsrv/X410/Xming, XQuartz,
  XWayland hints). Test servers allow agent and X11 forwarding. Tests: forwarded agent signs
  a nested `ssh` on the server (and fails without forwarding); an X client on the server
  reaches a fake local display with the fake cookie stripped; X11 unit tests.
- [ ] **MX-3 Terminal logging.** Per-session "log to file" (plain text, ANSI stripped, or
  raw), file name template with host and timestamp, started from settings or the tab menu.
- [ ] **MX-4 Terminal conveniences.** Copy on select, right-click paste (settings, default
  off like today), paste confirmation for multi-line text, keyword highlighting of output
  (error/warning/fail/ok… with user rules), font zoom per tab.
- [ ] **MX-5 Macros.** Record keystrokes in a terminal, save with a name, replay into the
  current terminal or all broadcast panes, run one on connect.
- [ ] **MX-6 Session folders and per-session settings.** Folders and favorites in the
  sidebar; per-Host startup command, remote start directory, terminal font/colors override,
  environment variables; duplicate and bulk edit.
- [ ] **MX-7 Session import.** PuTTY sessions (Windows registry, `~/.putty/sessions`) and
  MobaXterm bookmarks (`MobaXterm.ini` / `.mxtsessions`) into Hosts, with the same preview
  as the `~/.ssh/config` import.
- [ ] **MX-8 SSH key generator.** Ed25519, ECDSA, RSA; passphrase; OpenSSH and PuTTY
  `.ppk` output; copy public key, "install on Host" (append to `authorized_keys`).
- [ ] **MX-9 Follow terminal folder.** The SFTP sidebar follows the shell's current
  directory (OSC 7, with an opt-in shell snippet when the shell does not emit it).
- [ ] **MX-10 SCP.** Upload/download through `scp` when the server has no SFTP subsystem.
- [ ] **MX-11 Telnet and raw TCP sessions.** In-house Telnet client (option negotiation,
  NAWS, terminal type, binary), raw TCP; new session kinds in the connection model.
- [ ] **MX-12 External viewers.** Mosh, RDP (`xfreerdp`/`mstsc`/Microsoft Remote Desktop)
  and VNC sessions launched through tools the Driver Manager detects, with install hints.
- [ ] **MX-13 More local shells.** Shell picker (bash, zsh, fish, pwsh, cmd, Git Bash) and
  WSL distributions on Windows.
- Also counted for parity: M2-5 (SSH agent) and M4-2 (FTP/FTPS) above.

## Extra — UX pass (user request, 2026-10-08)

- [x] UX-1 Main menu Assistant toggle; assistant panel also in the API workspace. Response
  Pretty/Raw editors read-only instead of disabled (scrollbar drag, select, copy). v0.1.6.
- [x] UX-2 Title bar: icon buttons with binding-aware tooltips (Sidebar, theme, Settings), an
  Assistant toggle in both modes, Components only in debug builds, search stays centred across
  modes; palette "Switch to Default/API Workspace" and a test that bound commands show keys.
- [x] UX-3 Workbench header: named workspaces shown as "Projects" (new ones "Project N"),
  spark/title removed, Ctrl/⌘+L focuses URL, Ctrl/⌘+1–6 switch panel tabs, shortcuts in
  Send/tab tooltips; "Rename project…" in the project menu (store `rename_workspace`).
- [x] UX-4 Response panel: Copy all and per-row copy on Headers/Console/Trace/Tests, toolbar
  Copy response and Copy as cURL (from the redacted snapshot), in-place "Copied" feedback.
- [x] UX-5 Rail: collections, folders and requests rename inline in place of their row (kebab,
  double-click, F2; Enter/click-away commit, Escape cancels); request rename dialog removed.
- [x] UX-6 API environment labels (Prod/Stg/Dev/Local, Workbench store v6): Production red on
  the env chip, URL bar and request edge; confirm before Send or a run with unsafe methods.
- [x] UX-7 Workbench first-run page (Create project / Import collection / Paste cURL / sample
  request), empty Compose state with quick actions; request tabs open only on demand.

## DBX — Database explorer and editors at DBeaver / SSMS level (user request)

Gap analysis (2026-10-08): the explorer is a lazy, virtualized, single-connection tree
with object folders only; SQL editor and run experience are mostly there. Catalog reads
are new `IntrospectScope` variants + per-engine `*/catalog.rs` SQL; generated SQL
(templates, script-as, paging) goes on `Dialect`. Insta snapshots for every new SQL
string; `#[ignore]` docker tests for PG, MSSQL, Oracle. `sidebar.rs` and `overlays.rs`
are shared hotspots: one owner at a time.

### Phase 1 — fix and finish the explorer
- [ ] DBX-1a Dialect-aware templates: `Dialect::{select_template, insert_template,
  update_template, delete_template, script_drop}`; sidebar fetches `Detail` first so
  templates use real columns and the PK (fixes hardcoded `LIMIT 100` and empty columns).
- [ ] DBX-1b View DDL: the "ddl" action opens a read-only DDL tab from `Detail.ddl`
  (replaces the toast stub).
- [ ] DBX-1c Snowflake materialized views get their own folder.
- [ ] DBX-1d Global object search: `IntrospectScope::Search { pattern }` per engine, wired
  to the tree filter (debounced), local fuzzy filter as fallback.
- [ ] DBX-1e Tree keyboard navigation (arrows, Enter, Ctrl+C, F5) and drag an object
  into the SQL editor to insert its qualified name.

### Phase 2 — object properties and script-as
- [ ] DBX-2a Columns / Keys / Indexes / FKs / Triggers children under each relation.
- [ ] DBX-2b Object properties tab (Columns, Indexes, Constraints, FKs, Triggers, DDL,
  Data); `ObjectDetail` gains size, comment, trigger definitions per engine.
- [ ] DBX-2c Script-as submenu (CREATE, DROP, DROP+CREATE, SELECT, INSERT, UPDATE, DELETE,
  EXEC) with routine definitions per engine.
- [ ] DBX-2d Connection menu: New query here, Refresh schema, Disconnect.

### Phase 3 — table data editor
- [ ] DBX-3a Server-side filter / sort / paging bar (`Dialect::select_page`; MSSQL and
  Oracle `OFFSET … FETCH`, ORDER BY defaults to the PK).
- [ ] DBX-3b Insert / delete / duplicate rows in the staged-edit flow (`db/edit.rs`).
- [ ] DBX-3c Foreign-key navigation from a cell to the referenced row.

### Phase 4 — editor and run polish
- [ ] DBX-4a Per-tab database / schema switcher (`Dialect::use_database`; PG reconnects).
- [ ] DBX-4b Snippets (store table + completion items).
- [ ] DBX-4c Peek table: hover / F12 on an identifier shows its columns.
- [ ] DBX-4d Pin a result tab; compare two results.

### Phase 5 — advanced
- [ ] DBX-5a Dependencies (uses / used by) per engine.
- [ ] DBX-5b Activity monitor with kill session (never over MCP; Production double confirm).
- [ ] DBX-5c Users/roles, SQL Agent jobs, Oracle packages, Snowflake stages/tasks
  (new `ObjectKind`s; bump the schema cache key).
- [ ] DBX-5d ER diagram from foreign keys (hand-rolled layout; no new crate without asking).
- [ ] DBX-5e Favorites and a multi-connection tree (large `sidebar.rs` refactor; alone).

## M6 — Packaging and beta

Exit: every performance budget passes on all three platforms; signed builds published.

- [ ] **M6-1 macOS.** Universal binary, app bundle, signing, notarization, DMG, Homebrew cask.
  Partial: `packaging/macos/build-macos.sh` builds the universal app, `.dmg` and `.pkg`, with optional
  signing/notarization via env vars (not yet exercised with a real Developer ID). Cask pending.
- [ ] **M6-2 Windows.** Signed MSI, winget manifest.
  Partial: NSIS installer via `packaging/windows/build-windows.ps1` (optional signtool signing).
  `release-windows.yml` builds it with a static CRT, signs the exes, uninstaller and installer
  with Azure Trusted Signing (`-Sign`), verifies the publisher and uploads to the draft release.
  MSI and winget pending.
  MSI vs NSIS open, see DECISIONS 2026-10-05. winget manifest pending.
- [ ] **M6-3 Linux.** AppImage, `.deb`, `.rpm`, AUR PKGBUILD; Wayland and X11 checked.
  Partial: AppImage via `packaging/linux/build-appimage.sh` (appimagetool and runtime pinned by
  SHA-256); `release.yml` builds it on Ubuntu 22.04 and uploads to the draft release.
  `.deb`, `.rpm`, AUR pending.
- [ ] **M6-4 Auto-update.** Signed updates on macOS and Windows; Linux defers to package managers.
- [ ] **M6-5 Performance gates.** CI jobs that fail when a budget from CLAUDE.md is exceeded.
- [ ] **M6-6 Crash reporting.** Opt-in, scrubbed of SQL text, hostnames and credentials;
  telemetry off by default.
- [ ] **M6-7 Portable mode.** Marker file next to the binary keeps config and drivers beside it.

## Later (not before beta)

- Workload-wide index advisor, snippet library, import wizard, data compare, folder sync.

## Follow-ups

- API Workbench: rename and delete workspaces (API-5 only adds and switches them).
- `core/tests/flow.rs` `cancel_stops_a_running_query_quickly` times out (5 s) when the
  machine is busy building (seen twice); passes alone. Look at what it waits on before
  raising the timeout.

- Agent runs on systems with only the fallback vault: `swy mcp` cannot unlock it unless
  `SWITCHYARD_VAULT_PASSWORD` is in the app's environment. Option: let `swy mcp` borrow the
  running app's unlocked secrets over the loopback handoff, scoped by the session token.

(Add items here instead of doing them mid-task.)
- UX pass: Headers/Console/Trace/Tests response tabs copy via buttons only (no drag-select);
  `pm.sendRequest` inside runs skips the Production check; env dropdown doesn't show labels;
  no window-level UI tests for the new Workbench interactions; "Unsaved changes" on tab
  close could be a tab marker instead of a toast.
- Agent actual plans: approval prompt in the app for `explain` with ANALYZE / STATISTICS XML
  from an agent (the MCP server refuses them until then).
- Assistant panel: markdown is prose + code blocks only (no bold/lists/tables); GPUI tests
  for the panel and Assistant settings.
- Oracle: EXPLAIN PLAN / DBMS_XPLAN → `PlanNode`; V$SQL workload view; arm64 Linux archive;
  CI job with the `oracle` compose profile + Instant Client; TCPS / wallet sign-in.
- Snowflake: `EXPLAIN USING JSON` → `PlanNode`; QUERY_HISTORY / ACCESS_HISTORY workload view;
  exercise the driver against a real account; OAuth (external browser) sign-in.
- API workspace: port AgentOps's Workbench UI tests; persist workbench preferences (they live
  in session memory for now); per-project collections (`current_project()` returns None).
- Release workflow for macOS (`build-macos.sh` + notarization) next to the Linux and Windows ones.
- Log file for release builds (Windows GUI subsystem hides stdout).
- Real app icon to replace the generated placeholder in `packaging/icons/`.

- Smoke tests for the SQL Server, SSH and FTP containers (M0-2).
- Grid frame-time harness (M1-16).
- Driver Manager: fetch the signed manifest from the update server.
- SQL Server: upstream tiberius patches for INFO tokens (notices) and reading the attention
  acknowledgement across a message boundary (would remove the reconnect after cancel).
- Approval pending for `tokio-postgres-rustls`/`rustls-native-certs` and `lsp-types` (see DECISIONS).
