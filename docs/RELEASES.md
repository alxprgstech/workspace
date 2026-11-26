# Building and releasing ALXPRGS Workspace

A release is one signed zip containing the Windows binary with the embedded web
interface, both Linux runtime components and the deployment scripts. The target
machine never needs Rust, Node.js or a Linux toolchain.

## 1. Release contents

```text
workspace-<version>-windows-x64.zip
├── workspace-release.json      # signed manifest: version, commit, compatibility, hashes
├── workspace-release.sig       # base64 Ed25519 signature over the manifest bytes
├── workspace.exe               # Windows x64 binary with the embedded UI
├── linux/workspace-helper      # x86-64 musl, static
├── linux/workspace-egress      # x86-64 musl, static
├── deploy/install.ps1
├── deploy/provision-runtime.ps1
├── deploy/bootstrap-update.ps1
├── deploy/tunnel.ps1
├── deploy/alxprgs-quota
├── UPDATE.md
└── RELEASES.md
```

Excluded by construction: `config.toml`, `.env`, secrets, `data/`, databases,
workspace files, `target/`, `node_modules/`. The build script copies an explicit
allowlist of files; it never archives the repository.

The manifest pins:

- `format`, `product`, `version`, `git_sha`, `build_time_unix`, `channel`;
- `schema_version` (new database schema) and `compatibility`
  (`platforms`, `runtime`, `min_database_schema`, `max_database_schema`,
  `min_updater_format`);
- `changelog` and every file's `path`, `role`, `size`, `sha256`.

## 2. Toolchain and the `linker cc not found` failure

Building `workspace-helper` and `workspace-egress` for
`x86_64-unknown-linux-musl` on Windows fails because the host has no C linker.
The fix is to build inside a real Linux environment with an explicit linker:

```sh
export CC_x86_64_unknown_linux_musl=cc
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=cc
cargo build --release --locked -p workspace-helper -p workspace-egress \
  --target x86_64-unknown-linux-musl
```

`scripts/build-linux-musl.sh` runs exactly these commands, adds
`x86_64-unknown-linux-musl` to a rustup toolchain when needed, and refuses to
start if no `cc`/`gcc` is present.

Important detail discovered during acceptance: Debian's `musl-gcc` wrapper links
against the *system* musl dynamically and produces an ELF with
`interpreter /lib/ld-musl-x86_64.so.1`. The dedicated runtime does not have that
loader. The Rust musl target ships its own static libc and CRT objects; using the
plain Linux compiler (`cc`, i.e. GCC) produces a `static-pie` binary with no
INTERP segment. The build script and the release workflow verify that property
before an artifact is packaged. MSVC is never used for the Linux binaries and no
global Windows configuration is changed.

Prerequisites for a local release build:

- Windows with Rust 1.100 and Node.js 24;
- Docker Desktop (Linux containers) for the musl build, or a WSL/Linux machine
  that can run `scripts/build-linux-musl.sh`;
- the release signing key (see below).

## 3. Local release build

```powershell
npm --prefix web ci
npm --prefix web run typecheck
npm --prefix web run build
cargo build --release --locked -p workspace-cli --features web-ui

# one command for everything above, the musl build and the signed package:
powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1 -Version 0.2.0
```

Outputs:

```text
dist/workspace-0.2.0-windows-x64.zip
dist/workspace-0.2.0-windows-x64.zip.sha256
dist/release.json
```

Useful flags: `-SkipFrontend`, `-SkipWindows`, `-SkipLinux`, `-SkipPackage`,
`-AllowDirty` (test build from an uncommitted tree), `-ChangelogPath`,
`-LinuxImage`, `-SchemaVersion`, `-MinSchema`.

The script verifies the produced zip with the key embedded in the packaged
binary. A package that cannot verify is deleted from the release process, not
handed to an operator.

## 4. Signing keys

```powershell
workspace release keygen --out "$env:USERPROFILE\.alxprgs\release"
```

This writes:

- `release-signing-key.pk8` — PKCS#8 Ed25519 private key. Keep it outside the
  repository and never send it to a release artifact.
- `release-public-key.txt` — the public key. Commit it as
  `deploy/release-public-key.txt`; the updater embeds it at compile time.

Signing an existing manifest:

```powershell
workspace release sign workspace-release.json --key C:\keys\release-signing-key.pk8
```

Verification:

```powershell
workspace release verify --package dist\workspace-0.2.0-windows-x64.zip
```

`release verify` checks the signature first and only then parses the manifest and
hashes the files. Any unknown manifest field, duplicate role, absolute path,
`..` component or symbolic link is rejected.

Rotating the key requires shipping a build that embeds the new public key; an
update signed by the old key cannot install a later release signed by a new key.
Keep the previous key until the last installation has passed the rotation.

## 5. GitHub Actions release workflow

`.github/workflows/release.yml` runs only for:

- a `v*` tag push;
- an explicit `workflow_dispatch`, with optional `version`, `sign`, `publish`.

Normal pushes do not start a release build. Jobs:

1. `windows` — npm ci/typecheck/build, `cargo build --release --locked -p
   workspace-cli --features web-ui`, artifact `windows-binary`.
2. `linux` — `musl-tools`, `scripts/build-linux-musl.sh`, `file` checks for
   `ELF 64-bit`/`x86-64`, artifact `linux-binaries`.
3. `package` — assembles only the allowlisted files, resolves the version,
   signs and verifies the zip, writes the `.sha256` and uploads
   `release-package`.
4. `publish` — only when signing was requested and publishing is enabled (or a
   `v*` tag was pushed); creates the GitHub release with `contents: write`.
   Every other job has `contents: read`.

Signing in CI uses the repository secret
`RELEASE_SIGNING_KEY_PKCS8_BASE64` (base64 of the PKCS#8 file). Without the
secret a manual dispatch builds a **TEST** package signed with an ephemeral key.
The name contains `-TEST`, the verification step uses the ephemeral public key,
and installations that trust the embedded production key reject it. This is for
pipeline validation, not deployment.

The repository is public, so release assets can be downloaded without
credentials. For a future private repository the updater supports a token in the
environment variable named by `[update] api_token_env` (default
`WORKSPACE_UPDATE_TOKEN`); never put a PAT in the repository or in a package.

## 6. Publishing and installing

1. Build and verify the package locally (or let the workflow do it).
2. Attach `workspace-<version>-windows-x64.zip` and its `.sha256` to the GitHub
   release, or copy the zip to the target machine.
3. First installation of an old build:

   ```powershell
   powershell -ExecutionPolicy Bypass -File deploy\windows\bootstrap-update.ps1 `
     -Package C:\path\workspace-<version>-windows-x64.zip
   ```

4. Later updates: place the zip (or its extracted directory) at
   `<install_dir>/updates/package`, or pass `--package`, then run
   `workspace update`.

To use GitHub releases directly:

```toml
[update]
source = "github"
repository = "alxprgstech/workspace"
```

## 7. Reproducibility notes

- `Cargo.lock` is used with `--locked`; `npm ci` uses `package-lock.json`.
- The zip uses fixed timestamps, sorted entries and deterministic compression.
- The manifest's `build_time_unix` and the `-dirty` suffix are recorded, not
  hidden. Release packages must be built from a clean tree; `-AllowDirty` is for
  local test builds only.
- The Linux container image tag is recorded in `dist/release.json`. Pin a digest
  in `-LinuxImage` for a long-term reproducible rebuild.
- Rebuilding the same commit with the same toolchain produces byte-identical
  zips; the signature is deterministic for Ed25519.

## 8. Release checklist

- [ ] clean working tree at the intended commit;
- [ ] `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
      --all-features --locked -- -D warnings`, `cargo test --workspace
      --all-features --locked`;
- [ ] `npm ci`, `npm run typecheck`, `npm run build`, `npm test` in `web/`;
- [ ] `scripts/build-release.ps1` completes and verifies the package;
- [ ] package contents contain no configuration, secrets, databases or user
      files;
- [ ] test the package on an isolated instance (check, update, health, rollback);
- [ ] publish only with the owners' approval.
