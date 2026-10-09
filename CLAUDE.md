# CLAUDE.md — Switchyard

Switchyard is a fast, native, cross-platform desktop app that combines a database client
(PostgreSQL, SQL Server), SSH terminals with tunnels, and file transfer (SFTP, FTP, FTPS)
behind one connection model. It also visualizes query plans, finds optimization hotspots,
and exposes a `swy` CLI plus an MCP server so coding CLIs (Claude Code, Codex CLI, Gemini CLI,
or a custom one) can plan and optimize queries. Written in Rust with GPUI + gpui-component.

- Full product spec: `docs/SPEC.md` (source of truth for behavior and UX)
- Task plan: `PLAN.md` (work through it in order)
- Decisions log: `docs/DECISIONS.md` (append when you make or need a non-obvious call)

Engines: PostgreSQL, SQL Server, Oracle, Snowflake, Cloudflare D1 and local SQLite files. Oracle was moved
before beta at the user's request (see `docs/DECISIONS.md`); its client library (Instant
Client) is always runtime-loaded through the Driver Manager, never linked at build time.
Keep the `Driver` / `Dialect` traits general; engine settings without a field go in
`DbConfig::options`.

## How to work in this repo

1. Open `PLAN.md`, take the first unchecked task in the current milestone.
2. Before coding, state a short plan: files you will touch, types you will add, how you will test.
3. Implement only that task. No drive-by refactors; note them in `PLAN.md` under "Follow-ups".
4. Run the checks below. All must pass.
5. Tick the task in `PLAN.md` and add a one-line note (what landed, anything deferred).
6. If the spec is ambiguous or a task conflicts with a rule here, stop and ask. Record the answer in `docs/DECISIONS.md`.
7. When you learn something non-obvious (a crate quirk, a platform trap), add it to "Gotchas" below.

## Commands

```bash
cargo build --workspace
cargo run -p switchyard-app
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                              # unit tests, no network
docker compose -f docker/compose.yml up -d          # test services
cargo test --workspace -- --ignored                 # integration tests (need docker)
cargo bench -p switchyard-db                        # decode / grid benchmarks
```

Definition of done for any task: builds on stable, fmt clean, clippy clean with `-D warnings`,
unit tests pass, integration tests pass if the task touches a driver or protocol, PLAN.md updated.

## Workspace layout

```
switchyard/
  Cargo.toml              # workspace, shared [workspace.dependencies]
  rust-toolchain.toml     # pinned stable
  crates/
    app/       switchyard-app      GPUI binary: windows, panels, editor, grid, terminal view, file browser
    core/      switchyard-core     Hosts, connections, sessions, env rules, tokio runtime, event bus
    store/     switchyard-store    SQLite profiles, schema cache, query history, keychain/vault
    db/        switchyard-db       Driver/DbSession/Dialect traits, Value, RowBatch; pg + mssql modules
    remote/    switchyard-remote   SSH sessions, tunnels, SFTP, FTP/FTPS, RemoteFs trait
    term/      switchyard-term     Terminal state (alacritty_terminal), local PTY
    drivers/   switchyard-drivers  Driver Manager: manifests, detection, install, verify, runtime loading
    plan/      switchyard-plan     Plan capture, normalized PlanNode tree, findings rules, access-stats queries
    cli/       switchyard-cli      `swy` binary and MCP server (stdio)
    agents/    switchyard-agents   AgentAdapter trait, runner, Claude Code / Codex / Gemini / custom adapters
  docker/compose.yml      # postgres, mssql, openssh, ftp for integration tests
  docs/SPEC.md, docs/DECISIONS.md
```

Dependency direction: `app → core → {store, db, remote, term, drivers, plan, agents}` and
`cli → core`. `plan` depends on `db` only. `db` may depend on
`remote` only through the `TunnelEndpoint` type re-exported by `core`. Nothing depends on `app`.

## Architecture rules (do not break these)

- **The UI thread never does I/O.** All network and disk work runs on the tokio runtime owned by
  `switchyard-core`. The UI sends commands and receives events over channels. A GPUI task awaits
  the receiver and updates entities. No `block_on` anywhere in `app`.
- **Stream, don't materialize.** Query results flow as `ResultStream` events
  (`Columns`, `Rows(RowBatch)`, `Notice`, `NextResultSet`, `Done`). Batches of ~500–1,000 rows.
  The grid appends batches; it never waits for the full result.
- **Columnar results.** `RowBatch` stores typed column buffers; strings and bytes in an arena.
  No `Vec<Vec<String>>`, no per-cell allocation in hot paths.
- **Everything cancellable.** Every query and transfer holds a cancel handle reachable from the UI.
- **Dialect behind a trait.** Identifier quoting, catalog queries, LIMIT vs TOP, script splitting
  and batch separators live in `Dialect` impls. No `if engine == Postgres` in UI code.
- **Optional native libraries are runtime-loaded** with `libloading` through the Driver Manager.
  A missing library disables one feature; the app must still start. Do not link optional native
  libs at build time without an entry in `docs/DECISIONS.md`.
- **One SSH session per Host**, shared by terminals, SFTP and tunnels.
- **Virtualize every long list**: grid rows and columns, schema tree, file lists, scrollback.
- **One plan model.** PostgreSQL JSON plans and SQL Server showplan XML both convert into
  `PlanNode`. Findings rules and the plan UI only ever see `PlanNode`, never engine output.
- **The app and `swy` share one core.** The CLI and MCP server reuse `core` and `store`; no
  duplicated connection or query logic in `cli`.

## Agent safety rules (MCP server and assistant)

- Agent access is off by default, enabled per connection; Production excluded unless explicitly
  enabled, and then estimated plans and read-only tools only.
- `run_query` accepts only SELECT/WITH (checked with `sqlparser`), runs in `BEGIN READ ONLY`
  (PostgreSQL) or a rolled-back transaction (SQL Server), with a timeout and a row cap (default 200).
- Actual plans (`ANALYZE`, `STATISTICS XML`) from an agent need approval in the app. DML is always
  wrapped in a transaction and rolled back.
- Agents see connection names only: never hostnames, users, or secrets.
- No tool executes DDL. Index and statistics suggestions are returned as text.
- Every agent call is written to query history tagged `agent`.
- Safety lives in the MCP server, never in agent settings: every tool is read-only server-side,
  whatever a coding CLI is configured to allow.
- Coding CLIs run through `AgentAdapter` implementations (Claude Code, Codex CLI, Gemini CLI,
  custom). The UI only consumes normalized `AgentEvent`s; no CLI-specific code outside its adapter.
- Each run: private empty temp working directory (never a user project), MCP config written in the
  CLI's native format, short-lived `swy mcp` session token scoped to agent-enabled connections,
  token revoked and temp dir removed when the run ends.
- Where a CLI supports allow-lists or approval settings, permit only Switchyard's MCP tools and deny
  its shell and file-edit tools.
- Never read, copy, store or proxy a coding CLI's credentials. Each CLI uses its own login.
- History tags: `agent:claude-code`, `agent:codex`, `agent:gemini`, `agent:custom`.

## Coding conventions

- Edition 2024. Stable toolchain only.
- Errors: `thiserror` enums per library crate; `anyhow` only in `switchyard-app`'s top level.
- No `unwrap()` / `expect()` outside tests and one-time startup code.
- Logging with `tracing`. Spans per session and per query (id, engine, host alias — never credentials).
- Secrets are `secrecy::SecretString`. Never `Debug`, log, serialize, or put them in history/exports.
- Public types and traits get doc comments. Keep modules small; one driver per module.
- Tests: unit tests next to code; integration tests in `crates/*/tests/`, marked `#[ignore]`,
  using the docker services. Use `insta` snapshots for generated SQL and catalog output.

## Approved dependencies

Use these. Ask before adding anything else. Use the latest versions that compile together and
pin them in `[workspace.dependencies]`.

| Purpose | Crates |
| --- | --- |
| UI | `gpui`, `gpui-component` (versions must match each other) |
| Async | `tokio`, `tokio-util` (compat for tiberius), `futures` |
| PostgreSQL | `tokio-postgres`, `tokio-postgres-rustls`, `postgres-types` |
| SQL Server | `tiberius` (vendored; rustls, `winauth`, `sspi-rs` features → `winauth`, `sspi`) |
| SSH / SFTP | `russh`, `russh-sftp` |
| FTP / FTPS | `suppaftp` (async + rustls) |
| Terminal | `alacritty_terminal`, `portable-pty` |
| SQL tooling | `sqlparser`, tree-sitter + a SQL grammar (check what gpui-component bundles first) |
| Storage | `rusqlite` (bundled), `keyring`, `secrecy`, `argon2` + `chacha20poly1305` (fallback vault) |
| TLS / net | `rustls`, `reqwest` (rustls, for driver downloads), `minisign-verify` |
| Platform | `directories`, `libloading`, `chrono` (`clock` only: local time) |
| Serialization | `serde`, `serde_json` |
| Errors / logs | `thiserror`, `anyhow`, `tracing`, `tracing-subscriber` |
| Plans / CLI / MCP | `quick-xml` (showplan), `clap` (`swy`), `rmcp` (official Rust MCP SDK) |
| Diagrams | `dagre` (ER diagram layout; see `docs/DECISIONS.md`) |
| Testing | `insta`, `criterion`, `tempfile` |

## Licensing

The project must stay free of GPL code. GPUI and gpui-component are Apache-2.0 and fine.
Most other Zed crates (including its editor and terminal view) are GPL-3: do not copy or port
code from them. Check a crate's license before adding it.

## Security rules

- Passwords, passphrases, tokens: keychain or fallback vault only.
- TLS certificate verification on by default. Trust exceptions are per connection and pinned.
- SSH: strict host key checking; a changed key blocks the connection.
- Driver Manager downloads: verify signed manifest and SHA-256 before extracting.
- Production-labeled connections: confirm destructive statements (DROP, TRUNCATE,
  DELETE/UPDATE without WHERE), detected with `sqlparser`, not regex.

## Performance budgets (from SPEC; enforced in CI by M6)

Cold start < 500 ms · editor keystroke-to-frame < 8 ms · first rows visible < 50 ms after arrival ·
1M-row grid scrolls without dropped frames · idle memory with 3 tabs < 150 MB ·
1M rows × 10 numeric columns < 150 MB · no spinner for operations under 200 ms.

## Gotchas

- `EXPLAIN ANALYZE` and `SET STATISTICS XML ON` execute the statement. Never run them on DML
  outside a transaction that is rolled back.
- SQL Server returns showplan XML as a separate result set; the statement's own results arrive too.
  `SET SHOWPLAN_XML ON` must be alone in its batch.
- Coding CLI headless invocations (verify against the pinned versions; these change often):
  Claude Code `claude -p --output-format stream-json --mcp-config <json>`, resume `--resume <id>`;
  Codex CLI `codex exec --json`, MCP via `[mcp_servers]` in `config.toml`, resume `codex exec resume <id>`;
  Gemini CLI `gemini -p --output-format stream-json`, MCP via `mcpServers` in `.gemini/settings.json`.
- Claude Code 2.1 stream-json sends each content block of an assistant message as a separate
  `assistant` event with the same message id (not a growing message); don't diff by length.
  A `claude` started from inside a Claude Code session inherits its `CLAUDE_CODE_*`
  environment (same session id); harmless for tests, but don't assert on the id.
- procps `kill` ignores a negative pid unless it follows `--`: `kill -TERM -- -<pgid>`.
  Without it the call succeeds and signals nothing.
- Codex CLI `exec` has been reported to cancel MCP tool calls that need approval (no one can answer
  the prompt), and a generated `CODEX_HOME` hides the user's stored login. See PLAN task M5-12.
- Reading `shared_preload_libraries` with `current_setting` fails without `pg_read_all_settings`;
  `pg_settings` just hides the row. SQL Server records no missing indexes for trivial plans,
  so tests need a query that goes through full optimization (aggregate, ORDER BY).
- SQL Server 2022 turns Query Store on for new databases with `QUERY_CAPTURE_MODE = AUTO`,
  which skips cheap queries run a few times; tests that expect a query there set it to `ALL`.
- `pg_stat_statements`, HypoPG and Query Store are optional. Detect them and degrade gracefully;
  missing permissions (`pg_read_all_stats`, `VIEW SERVER STATE`) produce a hint, not an error.

- `gpui` and `gpui-component` versions must be compatible; check the gpui-component README for the
  matching `gpui` version before bumping either.
- `tiberius` takes a `futures` AsyncRead/AsyncWrite stream: wrap tokio streams with
  `tokio_util::compat` (`compat_write()`).
- `ssh-key` (russh's key parser) is vendored (`vendor/ssh-key`) to accept OpenSSH P-256 keys
  with a 31-byte scalar (one in 256); read `vendor/ssh-key/VENDORED.md` before bumping russh.
- `tiberius` is vendored (`vendor/tiberius`, patched via `[patch.crates-io]`) for the
  `AuthMethod::External` hook; read `vendor/tiberius/VENDORED.md` before updating it. Never
  enable its `integrated-auth-gssapi` feature (links GSSAPI at build time); Kerberos goes
  through `switchyard_drivers::gssapi`.
- `sspi` 0.18 (tiberius's pin) conflicts with russh over a `crypto-bigint` pre-release, so the
  vendored manifest uses 0.23; `picky-krb` 0.12.5 breaks `sspi` 0.23, so the lockfile pins 0.12.4.
- Apple's GSS headers pack `gss_OID_desc` / `gss_buffer_desc` to 2 bytes; MIT's Linux headers
  don't. The FFI structs in `drivers/src/gssapi.rs` follow that per platform.
- Kerberos needs the server's real name: the SPN is `MSSQLSvc/<server>:<port>` from the
  profile (not the tunnel's 127.0.0.1), so users must enter the FQDN, not an IP.
- `cargo test` exports `SSL_CERT_DIR` (the system store) to tests; a test that passes under cargo
  can fail TLS in the app or a bare binary. Point `SSL_CERT_FILE` at a test CA instead of
  installing it system-wide (that breaks `untrusted_certificate_is_refused`).
- gpui-kit's editor loses tree-sitter fold candidates for SQL while editing; the SQL tab sets
  its own with `apply_highlighter_fold_candidates` (`app/src/folds.rs`). Any other editor
  that needs folding must do the same.
- PostgreSQL cancel opens a second connection to the server. Through an SSH tunnel it must use the
  same tunnel endpoint.
- For SSH terminals, feed channel bytes into `alacritty_terminal`'s `Term` through its ANSI parser.
  Do not use its local tty module for remote sessions; that is only for the local PTY.
- Script splitting must respect strings, comments, PostgreSQL dollar-quoted bodies, and treat
  `GO` as a separator only when it stands alone on its line (SQL Server).
- `keyring` on Linux needs a Secret Service. Detect absence and fall back to the vault.
- Windows Credential Manager holds at most 2,560 bytes (1,280 UTF-16 units) per item; an Entra
  token cache is longer. `KeychainStore` splits long values over `<key>#1..n` items.
- GPUI: children of a scrolling flex column shrink to fit by default. A card with
  `overflow_hidden` inside one gets its bottom clipped instead of the column scrolling;
  give such children `flex_none()`. Text in a flex row needs `min_w_0()` to wrap.
- SSH agents: desktop-launched apps often lack `SSH_AUTH_SOCK`. 1Password's agent lives at
  `~/.1password/agent.sock` (macOS: `~/Library/Group Containers/2BUA8C4S2C.com.1password/t/agent.sock`),
  and OpenSSH configs point `IdentityFile` at a `.pub` to pick the agent key.
- Windows-only code can be type-checked from Linux per crate (not the whole workspace:
  `aws-lc-sys` needs a real MSVC toolchain): `rustup target add x86_64-pc-windows-msvc`, then
  `cargo clippy -p <crate> --target x86_64-pc-windows-msvc` with `CC_x86_64_pc_windows_msvc=clang`,
  `AR_x86_64_pc_windows_msvc=llvm-lib` and `CFLAGS_x86_64_pc_windows_msvc=-isystem <dir>` where
  `<dir>` holds stub `assert.h`/`string.h`/`stdlib.h` so `ring` compiles (check never links).
- PostgreSQL parallel plans: below a Gather, "Actual Total Time" is a per-process average and
  "Actual Loops" counts all processes, so time × loops overstates wall time; `plan::pg`
  divides by workers + leader.
- GPUI: an element's size is only known after a frame. Measure with a `canvas` prepaint into
  an `Rc<Cell<Bounds>>` and, when a layout depends on it (plan Fit), retry with
  `cx.on_next_frame`; reset the cell when the layout changes or it holds stale sizes.
- GPUI on Windows: the title bar row is a `WindowControlArea::Drag` (`HTCAPTION`), and GPUI
  reports it under every hitbox inside it. Every clickable title-bar control needs
  `.occlude()`, or Windows takes the click as a window drag. Linux/Xvfb does not show this.
- gpui-component 0.7 splits inputs: `InputState` (one line), `TextareaState` (multi-line),
  `EditorState` (code). Code ported from 0.5 (AgentOps) must pick the right one per field.
  `open_window` from gpui-kit already wraps the view in `Root`.
- The API script sandbox re-executes the app binary with `--switchyard-api-script-worker`;
  `main()` must check that argument before starting GPUI.
- Oracle Instant Client on Linux finds `libclntshcore` / `libnnz` only through the loader
  path, which glibc reads at process start (`libnnz.so` has no soname, so preloading it
  does not help). `main` calls `reexec_with_loader_path` to restart once with the client
  on `LD_LIBRARY_PATH`; child processes (PTY shells, package installs) get the user's
  original value back. It also needs the system `libaio.so.1`, which Ubuntu 24.04 renamed
  to `libaio.so.1t64` (Oracle's fix is a symlink). Oracle tests:
  `LD_LIBRARY_PATH=<ic> SWITCHYARD_ORACLE_CLIENT=<ic> cargo test -p switchyard-db --test oracle -- --ignored`.
- Docker Hub rate-limits anonymous pulls in CI and cloud sessions; gvenzl's Oracle images are
  also on ghcr.io.
- Pageant comes with russh on Windows (`AgentClient::connect_pageant`, `pageant` crate,
  Apache-2.0); no feature flag.
