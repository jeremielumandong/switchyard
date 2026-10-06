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
