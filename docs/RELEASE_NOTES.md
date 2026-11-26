# ALXPRGS Workspace 0.2.0

- Update system: signed release packages, verified installation and automatic rollback.
- New `workspace update` commands: `--check`, `--status`, `--rollback`, `--package`, `--bootstrap`.
- `workspace --version` now reports the build SHA and the installed release.
- Reproducible release builds for Windows x64 and Linux x86-64 musl (`workspace-helper`, `workspace-egress`).
- Release workflow for version tags and manual dispatches; normal pushes do not build a release.
- Administrator "Updates" section in the web interface (RU/EN, light/dark), backed by the same updater as the CLI.
- Authentication secrets are persisted through Windows DPAPI so an update never asks for them again.
- Database schema 9 → 11 migration is executed and verified by the updater with a pre-update snapshot.
- Fixed the broken `deploy/windows/provision-runtime.ps1` helper/egress installation block.
