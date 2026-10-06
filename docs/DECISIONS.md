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
