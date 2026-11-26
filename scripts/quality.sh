#!/usr/bin/env bash
# ALXPRGS Workspace — reproducible quality gate.
#
# Usage:
#   scripts/quality.sh            # Rust formatting, build, unit tests and clippy
#   scripts/quality.sh --full     # additionally the dependency audits and the web UI
#   scripts/quality.sh --e2e      # additionally the ignored runtime/MCP acceptance tests
#
# Every command is the exact one documented in docs/QUALITY.md. The script stops
# at the first failure and keeps a full log under target/quality/.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT}"

FULL=0
E2E=0

for argument in "$@"; do
    case "${argument}" in
    --full) FULL=1 ;;
    --e2e) E2E=1 ;;
    *) echo "unknown argument: ${argument}" >&2; exit 64 ;;
    esac
done

mkdir -p target/quality
LOG="target/quality/$(date -u '+%Y%m%dT%H%M%SZ').log"
exec > >(tee -a "${LOG}") 2>&1

step() {
    echo
    echo "==> $*"
}

step "Rust formatting"
cargo fmt --all -- --check

step "Rust build (locked, all features)"
cargo check --workspace --all-features --locked

step "Rust unit and integration tests (locked, all features)"
cargo test --workspace --all-features --locked

step "Rust lints (deny warnings)"
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

step "Whitespace and conflict markers"
git diff --check

if [[ "${FULL}" == "1" ]]; then
    step "Dependency audit (cargo-audit)"
    if command -v cargo-audit >/dev/null 2>&1; then
        cargo audit
    else
        echo "SKIPPED: cargo-audit is not installed (cargo install cargo-audit --locked)"
    fi

    step "Dependency policy (cargo-deny)"
    if command -v cargo-deny >/dev/null 2>&1; then
        cargo deny check
    else
        echo "SKIPPED: cargo-deny is not installed and no deny.toml is configured"
    fi

    step "Web UI: install, typecheck, build, audit"
    (
        cd web
        npm ci
        npm run typecheck
        npm run build
        npm audit
    )
fi

if [[ "${E2E}" == "1" ]]; then
    step "Runtime acceptance (real WSL2/rootless engine; non-destructive probe)"
    cargo run --locked -p workspace-cli --bin workspace -- --config config.toml runtime accept

    step "MCP protocol E2E (loopback listener, temporary database)"
    cargo test -p workspace-server --test mcp_e2e --all-features --locked -- --ignored --nocapture

    step "Runtime E2E (requires the dedicated runtime and its acceptance)"
    cargo test -p workspace-server --test runtime_e2e --all-features --locked -- --ignored --nocapture

    step "Recovery E2E (restarts the server on a temporary database)"
    cargo test -p workspace-server --test recovery_e2e --all-features --locked -- --ignored --nocapture

    step "Egress E2E (requires the configured allowlist proxy)"
    cargo test -p workspace-server --test egress_e2e --all-features --locked -- --ignored --nocapture
fi

step "Quality gate passed"
echo "Log: ${LOG}"
