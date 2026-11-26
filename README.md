# workspace

Lightweight, self-hosted execution platform for AI agents, built in Rust.
Isolated workspaces, modular capabilities, resource management, MCP integration,
and ALXPRGS SSO.

## Documentation

- [Architecture](docs/ARCHITECTURE.md) — runtime, isolation, storage and API design.
- [MCP interface](docs/MCP.md) — tools, scopes and the ChatGPT connector.
- [MVP scope](docs/MVP.md) — implemented stages and acceptance.
- [ChatGPT built-in environment](docs/CHATGPT_ENVIRONMENT.md) — dated inventory of
  the tools available in ChatGPT's own sandbox, compared with this Workspace, and
  the rules for choosing where a task runs.
- [Status report](docs/STATUS_REPORT.md) — the latest verified runtime state.
- [Completion report](COMPLETION_REPORT.md) — deployment and acceptance record.

## Quick start

```powershell
cargo build --release -p workspace-cli
target\release\workspace.exe --config config.toml runtime accept
target\release\workspace.exe --config config.toml serve
```

Runtime provisioning, deployment and rollback are described in
[deploy/windows](deploy/windows).
