# Vendored ssh-key 0.7.0-rc.11

Upstream: https://crates.io/crates/ssh-key/0.7.0-rc.11 (Apache-2.0 OR MIT, licenses alongside).
Used by russh for key files. Patched in through `[patch.crates-io]` in the workspace
`Cargo.toml`.

Copied: `src/`, `Cargo.toml` (minus `[[test]]` targets, whose files are not vendored),
licenses, README, CHANGELOG.

## Switchyard patch

`src/private/ecdsa.rs`, `EcdsaPrivateKey::decode` (marked "Switchyard patch"): accept a
private scalar shorter than 32 bytes. OpenSSH writes the scalar as a minimal `mpint`, so
about one P-256 key in 256 stores it in 31 bytes; upstream returned `Length` ("SshKey:
length invalid") for those keys, plain or passphrase-protected. The value is left-padded to
the field size as upstream already did for P-384/P-521. Still unfixed upstream (master) as of
2026-10-07. Regression test: `crates/remote/tests/keys.rs`.

To update: copy the new release over this directory, re-apply the patch if upstream has not
fixed it (search for "Switchyard patch"), and keep this file. When a release fixes it, drop
the vendored copy and the `[patch.crates-io]` entry.
