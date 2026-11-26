#!/bin/sh
# Build the two Linux runtime components as static x86-64 musl binaries.
#
# Run inside any Linux environment (native, WSL or the rust:1.100-bookworm
# container used by scripts/build-release.ps1):
#
#   sh scripts/build-linux-musl.sh
#
# Why a Linux linker is mandatory:
#
#   * the Windows host has no C linker at all ("linker `cc` not found"), and
#     MSVC cannot produce ELF;
#   * the Rust musl target ships its own static libc and CRT objects, so the
#     linker must be the Linux system compiler (`cc`, i.e. GCC), not MSVC;
#   * Debian's `musl-gcc` wrapper links against the *system* musl dynamically
#     (interpreter /lib/ld-musl-x86_64.so.1), which the dedicated runtime does
#     not have. The self-contained Rust target with plain `cc` produces a
#     static-pie binary that runs on a distribution without musl installed.
#
# The result is verified by scripts/build-release.ps1 and the release workflow:
# no INTERP segment, no dynamic dependencies.

set -eu

cd "$(dirname "$0")/.."

# Docker/CI images keep the Rust toolchain in /usr/local/cargo/bin. A login
# shell can replace PATH, so add it explicitly instead of assuming it.
if ! command -v cargo >/dev/null 2>&1; then
  PATH="/usr/local/cargo/bin:${HOME}/.cargo/bin:${PATH}"
  export PATH
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo is required (install the Rust toolchain)" >&2
  exit 2
fi

linker="cc"
if ! command -v "$linker" >/dev/null 2>&1; then
  linker="gcc"
fi

if ! command -v "$linker" >/dev/null 2>&1; then
  echo "a Linux C compiler (cc or gcc) is required for musl linking" >&2
  exit 2
fi

# Ensure the static target is present. This is idempotent and only applies to
# rustup-managed toolchains (the official containers and CI images use rustup).
if command -v rustup >/dev/null 2>&1; then
  if ! rustup target list --installed | grep -qx x86_64-unknown-linux-musl; then
    rustup target add x86_64-unknown-linux-musl
  fi
fi

export CC_x86_64_unknown_linux_musl="$linker"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$linker"

exec cargo build --release --locked \
  -p workspace-helper -p workspace-egress \
  --target x86_64-unknown-linux-musl
