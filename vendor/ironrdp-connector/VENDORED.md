# Vendored ironrdp-connector 0.10.0

Upstream: https://crates.io/crates/ironrdp-connector/0.10.0 (MIT OR Apache-2.0, licenses
alongside), part of https://github.com/Devolutions/IronRDP. Patched in through
`[patch.crates-io]` in the workspace `Cargo.toml`; used by the in-app RDP client
(`switchyard-remote`'s `rdp` module, `rdp-embedded` feature).

Copied: `src/`, `Cargo.toml` (crates.io's normalized manifest), licenses, README,
CHANGELOG. Not copied: `Cargo.toml.orig`, `Cargo.lock`.

## Why

Upstream 0.10.0 pins `picky = "=7.0.0-rc.25"` and `sspi = "0.21"` (with `scard`). The rest of
the tree already has `sspi` 0.23 and `picky` 7.0.0-rc.26 through the vendored tiberius (whose
pins must not move, see `vendor/tiberius/VENDORED.md` and the `picky-krb` 0.12.4 lock pin),
and sspi 0.21 / picky rc.25 would add a second sspi / picky / crypto stack that clashes with
russh's `curve25519-dalek` 5 and the `hmac` 0.13.0 pin. Moving this one crate to the tree's
versions resolves the clash without touching any existing pin; the other IronRDP crates
(`ironrdp-session`, `-pdu`, `-graphics`, `-input`, `-tokio`, `-async`, …) are used from
crates.io unchanged.

## Switchyard patch

Search for "Switchyard patch".

- `Cargo.toml`: `sspi` 0.21 → 0.23, `picky` `=7.0.0-rc.25` → `=7.0.0-rc.26`. Smart-card
  logon (sspi's `scard`, which pulls `winscard` and `cryptoki`) moved behind a new `scard`
  feature, off by default; `picky`, `picky-asn1-der` and `picky-asn1-x509` became optional
  (only that path uses them).
- `src/credssp.rs`: without `scard`, `Credentials::SmartCard` returns "smart card logon is
  not built in" and the smart-card helpers are compiled out. `TsRequest::buffer_len` is
  fallible in sspi 0.23 (it returned `u16` in 0.21); the error is mapped like the encode
  error next to it.
- `src/lib.rs`: `Credentials` gets a hand-written `Debug` without the password or PIN
  (`Config` and `ClientConnector` derive `Debug`, so the derived impl printed the secret).
- `src/connection.rs`: `#[expect(single_use_lifetimes)]` → `#[allow(…)]`; newer compilers
  no longer fire that lint there, and an unfulfilled `expect` warns.

To update: copy the new release over this directory (minus `Cargo.toml.orig` and
`Cargo.lock`), re-apply the patch, keep this file, and check `cargo tree -i sspi` /
`cargo tree -i picky` still show one version each. When upstream moves to the tree's
sspi / picky, drop the vendored copy and the `[patch.crates-io]` entry.
