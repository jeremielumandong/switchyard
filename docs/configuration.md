# Configuration and file locations

Where Switchyard keeps its files, how to move them, and the environment variables the app
and the `swy` CLI read. Both use the same files, so a connection saved in the app is
available to `swy` and the other way round.

## File locations

Switchyard uses three directories: **config**, **data** and **cache**. Their default places
follow each platform's conventions:

| | Linux | macOS | Windows |
| --- | --- | --- | --- |
| Config | `~/.config/switchyard` | `~/Library/Application Support/dev.Switchyard.Switchyard` | `%APPDATA%\Switchyard\Switchyard\config` |
| Data | `~/.local/share/switchyard` | `~/Library/Application Support/dev.Switchyard.Switchyard` | `%APPDATA%\Switchyard\Switchyard\data` |
| Cache | `~/.cache/switchyard` | `~/Library/Caches/dev.Switchyard.Switchyard` | `%LOCALAPPDATA%\Switchyard\Switchyard\cache` |

On Linux, `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `XDG_CACHE_HOME` replace `~/.config`,
`~/.local/share` and `~/.cache` when they are set.

What is in them:

| File or folder | Directory | Contents |
| --- | --- | --- |
| `switchyard.db` | config | Hosts, connections, settings, query history and saved plans, snippets, terminal macros, favorites, editor buffers, schema cache |
| `vault.json` | config | The encrypted vault, only used when there is no OS keychain (see [Secrets](#secrets)) |
| `api/` | data | API workspace: projects, collections, environments, request history and runs |
| `drivers/` | data | Components installed by the Driver Manager (for example Oracle Instant Client) |
| `logs/` | data | The app's log, `switchyard.log`, rotated by size |
| `known_hosts` | data | SSH host keys you trusted in Switchyard (your `~/.ssh/known_hosts` is read, never written) |
| `terminal-logs/` | data | Terminal session logs, unless a session log names another folder |
| `agent-tokens/`, `handoff.json`, `updates/` | data | Short-lived agent-run tokens (hashes only), messages from `swy explain --open` to the app, downloaded update installers |

Passwords, passphrases and tokens are not in any of these files when an OS keychain is
available. They are in the keychain (macOS Keychain, Windows Credential Manager, or the
Secret Service on Linux).

**Back up or move to another machine:** copy the config and data directories. Secrets in the
OS keychain do not move with them. Connection profiles can also be exported as JSON, always
without secrets.

**Remove everything:** uninstall the app, then delete the config, data and cache
directories, and remove Switchyard's entries from your keychain.

## Moving the files

Switchyard picks its directories in this order:

1. **Portable mode.** If a file named `switchyard.portable` sits next to the Switchyard
   executable, everything goes into a `switchyard-data` folder beside it, with `config`,
   `data` and `cache` subfolders. The release packages don't ship in portable mode yet; you
   create the marker file yourself.
2. **`SWITCHYARD_HOME`.** If this variable is set, everything goes under that folder, with
   the same `config`, `data` and `cache` subfolders.
3. Otherwise, the platform directories in the table above.

Set the same `SWITCHYARD_HOME` for the app and `swy` if you want them to share profiles.

## Secrets

By default, Switchyard stores secrets in the OS keychain. When none is available (for
example, on a minimal Linux system without a Secret Service), it uses the encrypted vault in
`vault.json` (Argon2 key derivation, ChaCha20-Poly1305), unlocked with a master password.

| Variable | Effect |
| --- | --- |
| `SWITCHYARD_SECRETS=vault` | Use the vault even when a keychain exists. Useful for `swy` on servers and in CI. |
| `SWITCHYARD_VAULT_PASSWORD` | `swy` only: unlock the vault with this password at start. |

## Environment variables

| Variable | Read by | Effect |
| --- | --- | --- |
| `SWITCHYARD_HOME` | app, `swy` | Put config, data and cache under this folder (see [Moving the files](#moving-the-files)). |
| `SWITCHYARD_SECRETS` | app, `swy` | `vault` forces the encrypted vault. Unset, Switchyard uses the keychain when there is one and the vault otherwise. |
| `SWITCHYARD_VAULT_PASSWORD` | `swy` | Unlocks the vault before running the command. |
| `SWITCHYARD_LOG` | app | Log filter in `tracing` `EnvFilter` syntax, for example `debug` or `switchyard_db=debug`. Default `info`. |
| `RUST_LOG` | `swy` | Log filter for `swy`, which logs to standard error. Default `error`. |
| `SWITCHYARD_NO_UPDATE_CHECK` | app | When set, no update check runs at startup. |
| `SWITCHYARD_THEME` | app | Start with this theme instead of the saved one: `dark`, `light`, `nord`, `dracula`, `catppuccin-mocha`, `tokyo-night`, `gruvbox-dark`, `high-contrast`, `catppuccin-latte`, `solarized-light` or `omarchy`. |
| `SWITCHYARD_ZOOM` | app | UI zoom factor, for example `1.5`, instead of the saved zoom. Clamped to the supported range. |
| `SWITCHYARD_AGENT` | `swy mcp` | Which coding CLI is calling: `claude-code`, `codex` or `gemini`; anything else is recorded as `custom`. Used for the `agent:<cli>` history tag. |
| `SWITCHYARD_MCP_TOKEN` | `swy mcp` | Set by the app for assistant runs it starts; limits the server to that run's connections. Don't set it yourself. |

Installer variables (`SWITCHYARD_VERSION`, `SWITCHYARD_APPIMAGE`) are described under
[Install (Linux)](../README.md#install-linux). Other `SWITCHYARD_*` variables in the source
are build settings or test settings, not user settings.

## Updates

Release builds check GitHub Releases for a newer version at startup and show a notice in the
status bar. Turn the startup check off in Settings or with `SWITCHYARD_NO_UPDATE_CHECK`;
**Check for Updates** (command palette, Help) still works. Switchyard doesn't replace itself
yet: install the new release the same way you installed the old one.

## Logs

The app writes its log to `logs/switchyard.log` in the data directory. **Open Log Folder**
(command palette, Help) opens it. Logs never contain passwords, tokens or keystrokes. See
[Troubleshooting](troubleshooting.md) for what to look for.
