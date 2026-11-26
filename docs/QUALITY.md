# Reproducible quality checks

Every check below is runnable from a clean checkout. `scripts/quality.sh`
executes exactly these commands, stops at the first failure and keeps a full log
under `target/quality/`. Nothing in this process needs production credentials.

```bash
scripts/quality.sh          # Rust formatting, build, tests, clippy
scripts/quality.sh --full   # + dependency audits and the web UI
scripts/quality.sh --e2e    # + the ignored runtime/MCP acceptance tests
```

## Prerequisites

| Tool | Version used | Needed for |
|---|---|---|
| Rust toolchain | `rust-toolchain.toml` (1.100.0) | all Rust checks |
| Node.js / npm | 24.20.0 / 11.19.0 | web UI checks |
| cargo-audit | latest (`cargo install cargo-audit --locked`) | `--full` |
| cargo-deny | optional, only with a `deny.toml` | `--full` |
| WSL2 + rootless Docker runtime | see `docs/ARCHITECTURE.md` | `--e2e` |

`cargo-deny` is intentionally not a hard requirement: there is no `deny.toml`
in this repository, and inventing a policy file without an ownership decision
would be misleading. The script reports it as skipped.

## Rust

```bash
cargo fmt --all -- --check
cargo check --workspace --all-features --locked
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
git diff --check
```

`--locked` is mandatory: a quality run must not silently update `Cargo.lock`.
`--all-features` includes the embedded web UI.

## Dependency audit

```bash
cargo audit      # advisories for Cargo.lock
npm audit        # advisories for web/package-lock.json
```

Policy: every reported advisory is either fixed by a compatible update, or
recorded here with the reason it is not exploitable in this deployment (for
example, a dependency that is only reachable in an optional feature that is not
built). An advisory is never ignored silently.

## Web UI

```bash
cd web
npm ci
npm run typecheck
npm run build
npm audit
```

`npm ci` (not `npm install`) keeps the lockfile authoritative.

## Database migrations

- `storage::initialize` migrates forward in one `IMMEDIATE` transaction and
  creates a `backups/pre-schema<N>-*` snapshot before any upgrade.
- Migrations are append-only; the current schema version is `10`.
- Regression tests apply the historical migrations one by one and assert the
  resulting version, preserved data and rollback on failure.
- A rollback of the binary requires restoring the matching database snapshot;
  migrations are not reversed in place.

## Failure and recovery checks

```bash
cargo test -p workspace-server --test recovery_e2e --all-features --locked -- --ignored --nocapture
```

This restarts the server on the same temporary database and asserts that an
active job becomes `interrupted`, the workspace survives, and deletion and
cleanup still work. The unit suite covers the same invariants for interrupted
deletions, failed container stops and failed storage release.

## Security regression tests

These run in the normal test suite and must stay green:

- scope and ownership checks for every MCP tool;
- OAuth token rotation, replay detection, expiry and revocation;
- PKCE and redirect-URI validation;
- CSRF and `Origin` checks on browser-facing endpoints;
- path traversal and symlink escape in the helper;
- archive extraction limits and traversal protection;
- SSRF protection in URL import (HTTPS only, public addresses, DNS pinning);
- egress allowlist enforcement.

## Integration tests that need the dedicated runtime

These are `#[ignore]`d so a unit-test run stays fast and host-independent:

```bash
cargo test -p workspace-server --test mcp_e2e --all-features --locked -- --ignored --nocapture
cargo test -p workspace-server --test runtime_e2e --all-features --locked -- --ignored --nocapture
cargo test -p workspace-server --test recovery_e2e --all-features --locked -- --ignored --nocapture
cargo test -p workspace-server --test egress_e2e --all-features --locked -- --ignored --nocapture
cargo test -p workspace-server --test image_build --all-features --locked -- --ignored --nocapture
cargo test -p workspace-cli --test runtime_accept --all-features --locked -- --ignored --nocapture
```

A skipped or ignored test is not a passing test. The release decision uses
`--e2e` on the release candidate.

## Last recorded run

| Date (UTC) | Commit | Rust tests | Clippy | Web | Audits | Runtime acceptance |
|---|---|---|---|---|---|---|
| 2026-11-26 | `38813b6` | 277 passed, 0 failed, 7 ignored | clean | typecheck + build, `npm audit`: 0 vulnerabilities | cargo-audit: not installed in this environment; npm: 0 | PASSED (8990 ms, real WSL2/rootless runtime) |

Update this table in the same commit that changes the result; a stale row is a
bug in the process.
