# Vendored tiberius 0.13.0

Upstream: https://crates.io/crates/tiberius/0.13.0 (MIT OR Apache-2.0, licenses alongside).
Patched in through `[patch.crates-io]` in the workspace `Cargo.toml`.

Copied: `src/`, `Cargo.toml` (minus `[[example]]`/`[[test]]` targets, whose files are not
vendored), licenses, README, CHANGELOG.

## Switchyard patch

`AuthMethod::External(ExternalAuthProvider)` with the `ExternalAuth` /
`ExternalAuthContext` traits (`src/client/auth.rs`) and its login arm
(`src/client/connection.rs`, marked "Switchyard patch"). It runs the same SSPI exchange
as upstream's `integrated-auth-gssapi` arm, but the tokens come from the caller. Switchyard
uses it for Kerberos on Linux/macOS with a GSSAPI library loaded at runtime, because the
upstream feature links libgssapi at build time (see `docs/DECISIONS.md`).

The new types are re-exported from `src/lib.rs`.

To update: copy the new release over this directory, re-apply the patch (search for
"Switchyard patch"), and keep this file.
