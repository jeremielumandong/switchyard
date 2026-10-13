# Troubleshooting

Each entry gives the symptom, the cause and the fix. If your problem isn't here, start with
the [log](#find-the-log).

- [Find the log](#find-the-log)
- [Starting the app](#starting-the-app)
- [Secrets and the vault](#secrets-and-the-vault)
- [SSH](#ssh)
- [Certificates and TLS](#certificates-and-tls)
- [SQL Server](#sql-server)
- [Oracle](#oracle)
- [Workload and plans](#workload-and-plans)
- [1Password secrets](#1password-secrets)
- [`swy`](#swy)

## Find the log

The app writes `switchyard.log` to the `logs` folder in its data directory (on Linux,
`~/.local/share/switchyard/logs`; other platforms are listed in
[Configuration](configuration.md#file-locations)). **Open Log Folder** in the command
palette (Ctrl/⌘+Shift+P) opens it.

For more detail, start the app with `SWITCHYARD_LOG=debug`. For a single area, use a
module filter, for example `SWITCHYARD_LOG=info,switchyard_remote=debug`. The log never
contains passwords, tokens or keystrokes.

## Starting the app

### Linux: the window doesn't open, or the log says "Failed to create surface"

**Cause:** Switchyard draws with the GPU through Vulkan, and no Vulkan driver is installed.

**Fix:** install your GPU's Vulkan driver and the Vulkan loader. On Ubuntu or Debian:
`sudo apt install libvulkan1 mesa-vulkan-drivers`. In a VM or under Xvfb, `mesa-vulkan-drivers`
provides a software driver (lavapipe). The other libraries the app needs are listed under
[Install (Linux)](../README.md#install-linux).

### Linux: the AppImage stops at once with a glibc version error

**Cause:** the release AppImage is built on Ubuntu 22.04 and needs glibc 2.35 or newer.

**Fix:** use a newer distribution, or build from source with `./install.sh --build`.

## Secrets and the vault

### Switchyard asks for a master password

**Cause:** there is no OS keychain. This is common on minimal Linux systems without a Secret
Service such as GNOME Keyring or KWallet. Switchyard then keeps secrets in its own encrypted
vault (`vault.json` in the config directory), locked with a master password.

**Fix:** none is needed; choose a master password and keep it safe. Settings shows which
backend is in use under *Secret storage*. To use the OS keychain instead, install and start
a Secret Service, then restart Switchyard.

### `swy` can't read passwords on a server or in CI

**Cause:** there is no keychain, and the vault is locked.

**Fix:** set `SWITCHYARD_SECRETS=vault` and `SWITCHYARD_VAULT_PASSWORD` for `swy`
([Configuration](configuration.md#secrets)).

## SSH

### Keys from my SSH agent aren't offered

**Cause:** apps started from a desktop launcher often don't get `SSH_AUTH_SOCK` from your
shell, so they can't find the agent. Switchyard also looks for 1Password's agent socket
(`~/.1password/agent.sock`, or
`~/Library/Group Containers/2BUA8C4S2C.com.1password/t/agent.sock` on macOS), the Windows
OpenSSH agent and Pageant.

**Fix:** start Switchyard from a terminal where `ssh-add -l` works, or set the Host to use a
key file instead of the agent. Settings → Drivers shows whether an SSH agent was detected.

### "Connection blocked: the host key for … has changed"

**Cause:** the server presented a different host key than the one saved for it. That
happens when a server is rebuilt, and also when someone intercepts the connection.

**Fix:** check with the server's administrator that the key really changed. If it did,
choose **Replace stored key…**; otherwise choose **Stay disconnected**. **Open known_hosts**
shows the stored entry. Switchyard never edits your own `~/.ssh/known_hosts`. Keys you
accept are stored in Switchyard's own `known_hosts` file.

## Certificates and TLS

### A server with a company or self-signed certificate is refused

**Cause:** Switchyard always verifies certificates against the operating system's trust
store. Importing a company CA in Switchyard, and trusting one server's certificate, are not
available yet.

**Fix:** add the company's root CA to the operating system's trust store (Keychain Access on
macOS, the certificate store on Windows, `update-ca-certificates` or your distribution's tool
on Linux), then reconnect. This applies to PostgreSQL, MySQL, Snowflake, D1, Redis, FTPS,
cloud services and the API workspace.

If you set `SSL_CERT_FILE` or `SSL_CERT_DIR`, Switchyard reads certificates *only* from there
and ignores the system store. The file must then include the public CAs too.

## SQL Server

### Windows / Kerberos (integrated) sign-in fails

**Cause:** Kerberos needs the server's real name to find its service principal
(`MSSQLSvc/<server>:<port>`). An IP address, or `localhost` through a tunnel, doesn't match.

**Fix:** enter the server's fully qualified domain name (for example
`sql01.corp.example.com`) in the connection, even when it is reached through a Host. On
Linux, the Driver Manager offers to install the Kerberos libraries if they are missing.

## Oracle

### Oracle connections fail on Linux after Instant Client was installed

**Cause:** Oracle Instant Client needs the system library `libaio.so.1`. Ubuntu 24.04 and
newer ship it as `libaio.so.1t64`.

**Fix:** install `libaio` (on Ubuntu, `libaio1t64`). If only `libaio.so.1t64` exists, add the
symlink Oracle recommends:
`sudo ln -s /usr/lib/x86_64-linux-gnu/libaio.so.1t64 /usr/lib/x86_64-linux-gnu/libaio.so.1`.
Then restart Switchyard.

## Workload and plans

### The Workload view shows a hint instead of statistics

**Cause:** the statistics need an extension or a permission the connection's user doesn't
have: `pg_stat_statements` (and `pg_read_all_stats`) on PostgreSQL, `VIEW SERVER STATE` or
Query Store on SQL Server.

**Fix:** the hint names the extension or the `GRANT` to run. Ask your database administrator
to run it, then refresh. Hypothetical indexes ("What if…") also need the HypoPG extension on
PostgreSQL.

## 1Password secrets

### A secret from 1Password doesn't load, or takes a long time

**Cause:** Switchyard reads 1Password secrets with the `op` CLI, which waits for you to approve
the request in the 1Password app. It gives up after 2 minutes. Apps started from the desktop
may also have a short `PATH` without `op`. Switchyard also looks in the folders the 1Password
installers use.

**Fix:** make sure `op` is installed and signed in, and approve the prompt in 1Password when it
appears.

## `swy`

### "this is a Production connection and the statement is destructive"

**Cause:** `swy` refuses DROP, TRUNCATE, and DELETE or UPDATE without WHERE on Production
connections unless you confirm. Nothing ran.

**Fix:** check the statement, then run it again with `--yes`. A connection locked read-only
refuses all writes, with or without `--yes`.

### `swy explain --open` says the plan can't be opened

**Cause:** history is off for that connection, so the plan wasn't saved.

**Fix:** turn history on for the connection, or use `--format json` to keep the plan
yourself.
