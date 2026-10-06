# Switchyard

A fast, native, cross-platform desktop app that puts database querying (PostgreSQL,
SQL Server), SSH terminals and file transfer (SFTP, FTP, FTPS) behind one connection model.
Written in Rust on [GPUI](https://www.gpui.rs/) and
[gpui-component](https://github.com/longbridge/gpui-kit).

- Product and technical spec: [`docs/SPEC.md`](docs/SPEC.md)
- Build plan and progress: [`PLAN.md`](PLAN.md)
- Decisions log: [`docs/DECISIONS.md`](docs/DECISIONS.md)
- Contributor / agent guide: [`CLAUDE.md`](CLAUDE.md)
- UI design prototype: [`docs/design/Switchyard.dc.html`](docs/design/Switchyard.dc.html)

## Quick start

```bash
cargo run -p switchyard-app          # the desktop app
cargo run -p switchyard-cli -- --help  # the `swy` CLI
cargo test --workspace               # unit tests (no network)
```

Linux builds need the usual GPUI system packages:

```bash
sudo apt install libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libxcb1-dev \
  libvulkan-dev libfontconfig-dev libfreetype-dev libssl-dev pkg-config
```

Integration tests run against the services in `docker/compose.yml`:

```bash
docker compose -f docker/compose.yml up -d
cargo test --workspace -- --ignored
```

## Workspace

| Crate | Responsibility |
| --- | --- |
| `switchyard-app` | GPUI application: windows, panels, editor, grid, terminal view, file browser |
| `switchyard-core` | Hosts, connections, sessions, environment rules, tokio runtime, event bus |
| `switchyard-store` | SQLite profiles, schema cache, query history, keychain / vault |
| `switchyard-db` | `Driver` / `DbSession` / `Dialect` traits, `Value`, `RowBatch`; PostgreSQL driver |
| `switchyard-remote` | SSH sessions, tunnels, SFTP, FTP/FTPS, `RemoteFs` |
| `switchyard-term` | Terminal state, local PTY |
| `switchyard-drivers` | Driver Manager: manifests, detection, install, verify, runtime loading |
| `switchyard-plan` | Plan model, findings rules |
| `switchyard-cli` | `swy` binary and MCP server |
| `switchyard-agents` | `AgentAdapter` trait and coding-CLI adapters |
