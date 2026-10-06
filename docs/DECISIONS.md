# Decisions log

Non-obvious calls made while building Switchyard. Newest last. Each entry: context,
decision, consequences. Entries marked **needs approval** add something CLAUDE.md asks to
confirm first; they are in place so the build works and can be reverted.

## 2026-10-05 — UI dependency is `gpui-kit` 0.7.1

The approved UI crates are `gpui` and `gpui-component`. gpui-component 0.7.1 depends on
`gpui-pre =0.3.8` (published snapshots of Zed's GPUI) and its platform bootstrap
(`application()`) lives in the `gpui-kit` facade, which re-exports exactly those two crates
plus `gpui-base` and the Lucide icon assets. The app depends on `gpui-kit =0.7.1` only, so
GPUI and gpui-component can never drift apart. All three are Apache-2.0.

## 2026-10-05 — Domain model lives in `switchyard-store`

PLAN M0-5 puts the model in core, but the dependency direction is `core → store`, and the
store must (de)serialize the model. The model (`Host`, `DbConnection`, `FileConnection`,
`TerminalProfile`, `EnvironmentLabel`, `Workspace`) therefore lives in
`switchyard_store::model` and is re-exported by core (`switchyard_core::store`). The store
depends on `switchyard-db` only for the small shared enums (`Engine`, `SslMode`,
`DbAuthMethod`).

## 2026-10-05 — `TunnelEndpoint` is defined in `switchyard-db`

`db` must not depend on `remote`, and `core` depends on `db`, so the type cannot live in
core either. It is a plain `{ host, port }` struct in `switchyard_db::driver`, re-exported
by core as `switchyard_core::TunnelEndpoint`; `remote` will produce values of it.

## 2026-10-05 — No `async-trait`; trait methods return `BoxFuture`

`Driver` and `DbSession` must be object-safe (`Box<dyn DbSession>`). Instead of adding
`async-trait`, methods return `futures::future::BoxFuture`. Same shape as the SPEC contract.

## 2026-10-05 — PostgreSQL rows are decoded from the binary wire format by hand

`tokio-postgres` maps types through `FromSql`, which allocates per cell for text and needs
`chrono`/`uuid` features for temporal types. The driver reads each cell as raw bytes
(`RawCell`, accepts every type) and decodes straight into the columnar `RowBatch`
(`pg::decode`): ints, floats, numeric (base-10000 digits → exact text), dates and
timestamps (PostgreSQL epoch → Unix epoch, infinities kept), uuid, json/jsonb, bytea,
interval, inet/cidr, arrays (recursive, quoted like psql), ranges, composites, enums,
domains and hstore. Unknown types fall back to UTF-8 text or hex. Measured: 1M rows × 3
columns stream in ~1.3 s locally; the first batch arrives in ~2 ms.

## 2026-10-05 — Result streaming uses a small first batch

Batches are 1,000 rows, but the first batch holds 200 and any partial batch is flushed as
soon as the socket would block. The grid paints the first rows immediately instead of
waiting for 1,000 rows on slow queries.

## 2026-10-05 — Multi-statement strings fall back to the simple protocol

Extended protocol rejects `SELECT 1; SELECT 2`. When prepare fails with that specific error
(and there are no parameters) the session re-runs the text through the simple protocol,
which yields text-typed columns and one result set per statement.

## 2026-10-05 — Parameters are bound by server-inferred type from text

Parameter prompts produce text. `PgParam` encodes a `Value` for whatever type the server
inferred at prepare time (bool, ints, floats, numeric, uuid, bytea, json/jsonb, date, time,
timestamp[tz], text-like, enums, domains). Anything else returns a clear error asking the
user to cast the placeholder (`$1::text`). Values are never spliced into SQL.

## 2026-10-05 — TLS: `rustls` with the `ring` provider and native roots (**needs approval**)

Verification needs root certificates. `rustls-native-certs` (MIT/Apache) loads the OS
store; it is not on the approved list. The `ring` provider avoids `aws-lc-rs`, which needs
CMake/NASM on Windows. `SslMode::Require` verifies certificates too (stricter than libpq),
per the security rule "verification on by default".

## 2026-10-05 — Random ids and salts come from the AEAD crate

No `rand`/`uuid` dependency: `chacha20poly1305::aead::Generate` (OS CSPRNG via `getrandom`)
produces profile ids (16 hex chars) and the vault's salts and nonces.

## 2026-10-05 — Secrets backend selection

`KeychainStore` (keyring v4, `v1` feature) when `keyring::Entry::store_status()` is OK;
otherwise the encrypted vault (`argon2id` → ChaCha20-Poly1305, one nonce per entry, file
written atomically with mode 0600). `SWITCHYARD_SECRETS=memory` selects an in-memory store
for demos and tests. Logs contain key names only; a test captures tracing output and
asserts the secret and master password never appear.

## 2026-10-05 — One session per SQL tab, one catalog session per explorer

Each SQL tab opens its own session so tabs run queries concurrently and transactions stay
per tab. The schema explorer opens a separate session for the active tab's connection so
introspection never waits behind a long query.

## 2026-10-05 — Production guard runs in the UI and again in core

The UI pre-checks with `guard::classify` (sqlparser) to show the confirmation dialog before
anything is sent; core re-checks every statement and refuses unconfirmed destructive
statements on Production and any write on a read-only connection. Unparseable statements
are never treated as read-only.

## 2026-10-05 — Results grid is gpui-component's `DataTable`

`DataTable` virtualizes rows and columns, supports fixed, resizable and movable columns,
sorting hooks and cell selection. `GridDelegate` reads cells from the columnar
`BatchList` and formats only visible cells. Sorting and filtering keep a permutation
vector; the data is never copied.

## 2026-10-05 — Fetch limit pauses the stream instead of dropping it

When a result set reaches the fetch limit (default 10,000), core stops polling the stream
and waits; TCP back-pressure holds the server. "Fetch all" resumes; Cancel sends the
server cancel and drops the stream.

## 2026-10-05 — Bundled fonts: Geist and Geist Mono (OFL-1.1)

The design uses Geist. The regular/medium/semibold/italic cuts are embedded (~1.1 MB) and
registered at startup; the OFL license text ships next to them in `crates/app/assets/fonts`.

## 2026-10-05 — Milestones beyond M1 show honest placeholders

Terminal (M2), SQL Server (M3), file transfer (M4) and agents (M5) are not implemented
yet. Their UI surfaces exist (tabs, connection types, Settings → Drivers), but they state
what is missing instead of showing mock data. Test connection for those types says which
milestone delivers them.

## 2026-10-05 — `lsp-types` for SQL completion (**needs approval**)

gpui-component's editor takes completions through its `CompletionProvider` trait, whose
signature uses `lsp-types` (MIT) item types. The app depends on the same version
gpui-component uses so completion items can be constructed.

## 2026-10-06 — M1-16 benchmark numbers

Measured on the Linux build container (release profile, criterion):

| Measurement | Result | Budget (CLAUDE.md) |
|---|---|---|
| Decode 1,000 rows × 5 columns into `RowBatch` | ~99 µs | — |
| Look up and format one screen (40 rows × 10 columns at row 500k of 1M) | ~16 µs | no dropped frames (16 ms) |
| 1,000,000 rows × 10 `int8` columns held in `BatchList` | < 150 MB (asserted by test) | < 150 MB |

The grid frame-time harness is still missing (see PLAN Follow-ups); the numbers above cover
decode and the per-frame formatting work only.

## 2026-10-06 — Cloudflare D1 as a third engine (user request)

The user asked for a client for Cloudflare D1 through the REST `raw` endpoint
(`POST /accounts/{account_id}/d1/database/{database_id}/raw`). This extends the scope in
CLAUDE.md (PostgreSQL, SQL Server) at the user's request.

- `Engine::D1` with a SQLite dialect (`dialect/sqlite.rs`): `"ident"`, `[ident]` and
  `` `ident` `` quoting, flat block comments, `CREATE TRIGGER … BEGIN …; END` kept whole,
  and every placeholder (`?`, `?N`, `:name`) rewritten to `?N` because D1 binds positional
  parameters only.
- Profile mapping without new fields: `server` = account id, `database` = database id, the
  keychain secret = API token. Validation skips host/user for cloud engines and rejects
  "via Host".
- HTTP goes through `reqwest` (approved for driver downloads) with `rustls-no-provider` and
  the same ring + native-roots `rustls::ClientConfig` the PostgreSQL driver uses
  (`db::tls::client_config`), so there is one TLS stack and verification is always on. The
  token travels only in a header marked sensitive.
- The API returns column names but no types; types are inferred per column from the JSON
  values (integer, real, text, blob-as-byte-array, JSON, mixed → text).
- D1 has no interactive transactions: `begin` returns `Unsupported` and the editor refuses
  manual mode for engines where `Engine::supports_transactions()` is false.
- Cancel abandons the HTTP request; D1 has no server-side cancel, so a statement that
  already reached the database still finishes there.
- Results carry no source-table ids, so inline editing is not offered for D1.
- Each result adds a notice with rows read/written, database time and serving region (D1
  bills by rows read).
- Tested against a local stand-in for the endpoint (`crates/db/tests/d1.rs`). Against the
  real API, a test connection with a fake token returned "Authentication error" and was
  reported correctly (URL, TLS, envelope parsing verified); a successful query against a
  real database still needs a real account.

## 2026-10-06 — Azure SQL with Microsoft Entra ID (user request, pending M3)

The user wants SQL Server connections to Azure SQL with Entra ID (Azure AD) login,
including MFA. Plan for M3: tiberius `AuthMethod::AADToken` with tokens from the Microsoft
identity platform over `reqwest` — interactive (authorization code + PKCE through the
system browser and a loopback redirect) and device-code flows both satisfy MFA; password
(no MFA) and service-principal flows for automation. Refresh tokens go to the keychain.
**Open question:** which Entra application (client id) to use — a Switchyard app
registration, or one the user supplies per connection.

## 2026-10-06 — Terminal architecture

- `alacritty_terminal::Term` lives behind its `FairMutex`, shared by a `Feeder` (I/O side,
  parses bytes) and `Terminal` (UI side, copies the visible screen into a `Snapshot`).
  The UI is woken by one coalesced `TerminalWake` per batch of output, so a flood of
  output costs at most one snapshot per frame.
- Terminal input and resize commands are applied directly on the core command loop (not on
  spawned tasks): spawned tasks reordered keystrokes.
- Inside a terminal, Ctrl+letter goes to the program. App shortcuts there are Cmd+… on
  macOS and Ctrl+Shift+… elsewhere (copy C, paste V, find F, split D); the global
  Ctrl+P/N/B/W/./, bindings are unbound in the `Terminal` key context.
- Keystrokes are never logged at any level (they can contain passwords typed at prompts).
- OSC 52 clipboard writes from programs are honoured; clipboard reads are not offered.
- The view is our own canvas element (no code from Zed's GPL terminal view).

## 2026-10-06 — SSH client details

- `russh` is built with the `ring` backend (not its default `aws-lc-rs`), so the whole app
  has one crypto provider.
- Host keys: our own `known_hosts` reader (`remote/src/ssh/known_hosts.rs`) skips lines it
  cannot parse instead of failing the whole lookup (russh's reader does the latter, which
  would turn a changed-key block into an "unknown key" prompt). It handles hashed hosts
  (`|1|salt|hash`), wildcards, negation and `@revoked`. The user's `~/.ssh/known_hosts` is
  read only; trusted keys go to Switchyard's own file. This uses `hmac` 0.13 + `sha1` 0.11
  + `data-encoding` (already in the tree through russh) — **needs approval**.
- One session per Host: `SshManager` keeps a weak reference per Host id behind a per-Host
  async lock, so two terminals opening at once share one login. Jump hosts are sessions
  too, kept alive by the sessions that ride on them.
- Host certificates (`@cert-authority`) are not supported yet; such servers are refused.
- Windows agent support uses the OpenSSH agent named pipe; Pageant is not wired yet.
- Integration tests run against local `sshd`s started by `scripts/ssh-test-servers.sh`
  (also in CI). The docker `openssh` service is not used by them.

## 2026-10-05 — Local packaging scripts ahead of M6 (Windows uses NSIS, not MSI)

Packaging scripts landed early, on request, in `packaging/`: a universal macOS `.app` packed as
`.dmg` and `.pkg`, an NSIS installer for Windows, and an AppImage for Linux. SPEC and PLAN M6-2
say "signed MSI"; the NSIS installer is what was asked for now. Revisit before M6-2 whether
MSI is still needed (enterprise GPO deployment) or NSIS replaces it.

- Signing and notarization run only when their environment variables are set (see each script's
  header); without them the macOS app is ad-hoc signed and Windows binaries are unsigned.
- Bundle / app id: `io.github.jeremielumandong.switchyard`.
- The icon in `packaging/icons/` is a generated placeholder (`generate.py`); swap in real artwork
  as `switchyard.png` (1024²) and re-run the generator for the `.ico` and 256 px PNG, or replace
  them directly.
- Release Windows builds use `windows_subsystem = "windows"`, so `tracing` output to stdout is
  not visible there; a log file is a follow-up.
- The AppImage does not bundle GPUI's system libraries or glibc; build it on the oldest
  distro to support.

## 2026-10-06 — gpui-fast (Retained Mode) under GPUI Kit; Rust 1.98.1

At the user's request the app runs on [gpui-fast](https://github.com/longbridge/gpui-fast)
(Apache-2.0), a GPUI fork that only redraws views that changed. It ships `gpui-pre`
compat crates with the exact names and version GPUI Kit pins (`=0.3.8`), so the switch is a
`[patch.crates-io]` of `gpui-pre`, `-platform`, `-macros`, `-sum-tree` and `-web`, pinned to
rev `598306f`; no app code changed for it. When GPUI Kit moves to a newer `gpui-pre`, move
the rev with it (Cargo warns "patch not used" until then).

- The toolchain moved to Rust 1.98.1 to match gpui-fast.
- `unicode-properties` is locked at 0.1.3 because gpui-fast's `gpui_web` pins `=0.1.3`.
- Retained rendering does not see state a view reads outside entities. Two places read
  the clock and now notify on a timer: the SQL tab's elapsed time while a query runs
  (250 ms) and the workspace's relative times such as "Cached 3 min ago" (30 s).
- Checked in the app: query streaming and timer, grid scrolling, terminal (`htop`), theme
  switch. No frame-time comparison yet (the grid frame-time harness is still a follow-up).

## 2026-10-06 — SSH tunnels

- A tunnel is a local listener on `127.0.0.1:<ephemeral>`; every accepted connection gets
  its own `direct-tcpip` channel on the Host's shared session. Drivers only ever see a
  `TunnelEndpoint`, so PostgreSQL's cancel request (a second TCP connection) goes through
  the same tunnel automatically.
- Core keeps one tunnel per (Host, target host, target port), shared by every session that
  needs it; creation is serialized so a SQL tab and the schema explorer connecting at the
  same moment do not open two. Sessions hold the tunnel; it closes with its last session.
- A tunnel holds its SSH session while it exists, and a new connection after a drop logs
  in again (prompting if the Host needs it). Its status is Reconnecting until then.
- Stopping a tunnel aborts its forwarded connections and ends every session that used it,
  with a message naming the Host and port.
- New SSH sessions stay up for 60 s even with no users, so "Test connection" followed by
  "Save and connect", or reopening a terminal, does not ask for a second MFA code.

## 2026-10-06 — SQL Server driver (tiberius)

- `tiberius` 0.13 with `tds80` and `rustls`. Its rustls feature also builds `aws-lc-rs`;
  `db::tls::install_default_provider` makes ring the process-wide provider so every driver
  uses one TLS stack. Per-connection trust is `DbConfig.trusted_ca_pem` (M3-8 adds the UI).
- tiberius' result stream borrows the client mutably, so one task per session owns the
  client and serves requests in order (queries, transaction batches, catalog). Results go
  through a channel of 4 batches, so a paused grid slows the server down through TCP.
  The first batch is 200 rows, then 1,000.
- tiberius drops DONE row counts, so after a batch without result sets the driver runs
  `SELECT CAST(@@ROWCOUNT AS bigint)` for "rows affected".
- tiberius does not surface INFO tokens (`PRINT`, `RAISERROR` below severity 11), so SQL
  Server notices are not shown yet. Follow-up: upstream patch.
- **Cancel (M3-2).** Cancel sends the TDS attention (`cancel_query`). SQL Server 2022/2025
  acknowledges it in a TDS message of its own after the message that ends the cancelled
  batch. tiberius stops at the first message boundary and reports "Never got a DONE token
  acknowledging the Attention signal", leaving the connection one response behind; its
  public API has no way to read a message without sending a request. Chosen: send the
  attention (so the server stops at once), then, if the acknowledgement was not read,
  reconnect with the same config and tunnel endpoint, and emit a warning notice saying the
  open transaction, temp tables and SET options were reset. The session's
  `in_transaction()` clears. `WAITFOR DELAY '00:00:30'` stops in about 0.3 s after cancel.
  Upstream fix (keep reading past the message boundary until the attention DONE) would
  remove the reconnect; tracked in Follow-ups.
- Azure SQL gateway redirects (`Routing`) are followed up to three times when not tunnelled.

## 2026-10-06 — Microsoft Entra ID sign-in (M3-9)

- **Client id (user decision):** one built-in Switchyard app registration, multi-tenant
  public client, set at build time (`SWITCHYARD_ENTRA_CLIENT_ID`, see `docs/entra-app.md`).
  A connection can override it with its organization's own application id. Builds without
  it say so and point to the override.
- Flows, all against `login.microsoftonline.com/<tenant>/oauth2/v2.0`, scope
  `https://database.windows.net//.default` (+ `offline_access` for user flows):
  browser (authorization code + PKCE S256, loopback redirect `http://localhost:<port>`,
  state checked, other requests to the port ignored), device code, password (ROPC, no MFA)
  and client credentials (service principal). Tenant defaults to `organizations`.
- The token goes to tiberius as `AADToken`. Access tokens are cached in memory per
  connection until 5 minutes before expiry; refresh tokens are stored in the keychain/vault
  under `<connection id>:entra-refresh` and tried before any prompt, so the browser only
  opens when the refresh token has expired or been revoked. Deleting the connection
  removes it. One sign-in at a time, so two tabs connecting together open one browser.
- Prompts reuse the runtime prompt queue: `EntraSignIn` (the app opens the URL and shows
  Cancel / Copy link / Open again) and `EntraDeviceCode` (code, Copy code, Open page);
  `PromptClosed` withdraws them. 5-minute limit per sign-in.
- **Needs approval:** `ring` as a direct dependency of `switchyard-db` for SHA-256 and the
  CSPRNG (PKCE verifier, state). It is already in the build as rustls' crypto provider.
- Known limit: a session reconnecting after a cancel (SQL Server) reuses the token it
  connected with; after about an hour that reconnect fails and the tab must reconnect.

## 2026-10-06 — Driver Manager (M3-4 to M3-6)

- **Manifest:** the SPEC format (`id`, `required_by`, `detect`, per-platform `strategy`),
  plus `license` (`accept_required` for click-through terms) and `detect.env` /
  `detect.min_version`. Strategies: `builtin`, `package` (per-manager names), `archive`
  (version, URL, SHA-256, size, library folder) and `manual` (steps). The manifest compiled
  into the app (`crates/drivers/manifest.json`) is trusted; a downloaded one is used only
  when its minisign signature verifies against `SWITCHYARD_MANIFEST_PUBKEY` (build-time).
  Only prehashed (current) minisign signatures are accepted. The update-server fetch is a
  follow-up: it needs the maintainers' signing key and a URL.
- **Detection order:** a path the user chose (`<drivers>/paths.json`), the app-managed
  directory (`<data>/drivers/<id>/<version>/`, newest first, minimum version enforced),
  built into the OS, environment variables, then system library directories.
- **Install:** packages run through `pkexec` on Linux (the desktop's password dialog);
  without it the card shows the exact `sudo …` command to run in a terminal and a Re-check.
  Homebrew runs unelevated; winget elevates itself. Archives download with reqwest + the
  app's rustls config, are hashed while on disk, refused on any SHA-256 mismatch, unpacked
  into a staging folder and renamed into place. Install from file uses the same check.
  A mirror setting (`drivers.mirror`) replaces the archive URL's origin, keeping the file
  name. Loading uses `libloading` in one `#[allow(unsafe_code)]` function.
- **Archive reader:** our own small ustar/GNU/pax reader rather than the `tar` crate (not
  on the approved list). It refuses absolute paths, `..`, links leaving the folder, writes
  through earlier links, hard links and devices, and caps the unpacked size at 4 GB.
  **Needs approval:** `flate2` (gzip), already in the build through russh.
- Debug builds only: `SWITCHYARD_DEV_MANIFEST=<file>` loads an unsigned manifest to try
  the archive flow locally.

## 2026-10-06 — Themes (user request)

- Ten built-in themes: Switchyard Dark/Light (the design), Nord, Dracula, Catppuccin Mocha
  and Latte, Tokyo Night, Gruvbox Dark, Solarized Light and High Contrast. The community
  palettes are MIT-licensed color values, adjusted where needed for contrast.
- A unit test holds every theme to: text 7:1 on all backgrounds and on the selection,
  secondary text 4.5:1, muted text 3:1, syntax colors 4.5:1, selection visibly different
  from the background. New themes must pass it.
- The choice is saved in the `theme` setting and applied when the app starts
  (`SWITCHYARD_THEME` still overrides it, for tests and screenshots).

## 2026-10-06 — SSH files: explorer, transfers, remote editing (user request, M4 early)

- The user asked for a file explorer tied to the SSH connection, copying from their
  computer, and editing remote files, next to the terminal. Built ahead of the M4 order:
  SFTP (`russh-sftp`) runs as a channel on the Host's shared SSH session, so the terminal,
  tunnels and files share one login. One SFTP session per Host, reopened if the SSH session
  dropped.
- UI: with an SSH terminal (or a file from that Host) in front, the sidebar's second tab
  reads **Files** instead of Schema and browses the Host. Upload by dropping files or
  folders from the OS onto it or with Upload (system picker); download goes to
  `~/Downloads` (keep both on name clash); delete asks for a second click. Uploads ask
  before replacing (Replace / Keep both / Skip).
- Editing: text files up to 5 MB (no NUL bytes, UTF-8) open in an editor tab. Save checks
  the file's modification time first; if it changed on the server the tab offers
  Overwrite or Discard and reload. Closing a tab with unsaved changes needs a second click.
- Still to do in M4: transfer queue limits, pause and resume from offset, the dual-pane
  Files tab, FTP/FTPS, chmod.
