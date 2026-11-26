# Updating ALXPRGS Workspace

The updater replaces the Windows binary and every Linux runtime component as one
transaction. It verifies a signed release, snapshots the installation, migrates
the database, restarts the server, verifies it and rolls everything back if a
step fails.

## 1. Commands

```powershell
workspace --version                     # build SHA and installed release
workspace update --check                # is a newer verified release available?
workspace update                        # install the available release
workspace update --status               # journal, progress, rollback availability
workspace update --rollback --yes       # restore the previous release
workspace update --rollback --yes --allow-data-restore
workspace update --package C:\release\workspace-0.2.0-windows-x64.zip
workspace update --check --json         # machine-readable output
```

`workspace update` without arguments uses the configured package source:

| `[update] source` | Behaviour |
|---|---|
| `local` (default) | Reads `<install_dir>/updates/package` or `update.package_dir` |
| `github` | Downloads the latest release asset of `update.repository` |

A release is never installed without a valid Ed25519 signature over the manifest
and matching SHA-256 for every file. A corrupted or unsigned package is rejected
before the server is stopped.

## 2. First update of an old installation

Installations from commit `660023c` and earlier do not contain the `update`
command. Use the new binary from the release package:

```powershell
powershell -ExecutionPolicy Bypass -File deploy\windows\bootstrap-update.ps1 `
  -Package C:\path\workspace-0.2.0-windows-x64.zip
```

The script verifies the package with the new binary, checks compatibility and
runs the same update engine in foreground mode. The old Windows image is not the
running image, so it can be replaced directly. Every later update uses the
ordinary `workspace update` command.

The updater needs the two authentication secrets only to restart the server:

- `WORKSPACE_SSO_CLIENT_SECRET` and `WORKSPACE_AUTH_KEY` from the environment, or
- the DPAPI-protected copy that the running server writes to
  `<data_dir>/state/secrets.dat`.

It never prompts for them and never prints them. If neither source is available
the update stops before changing anything.

## 3. What an update does

1. **Preflight** — reads the package, verifies the signature and hashes,
   checks the release platform/runtime/database compatibility, free space on the
   Windows volume and the WSL filesystem, the WSL distribution and component
   directories, and the availability of the secrets.
2. **Lock** — `<install_dir>/state/update/update.lock` makes a second update
   impossible. A lock owned by a dead process is reclaimed.
3. **Staging** — the verified package is copied into
   `<install_dir>/updates/staging/<journal>` and verified again.
4. **Drain** — queued/running jobs are allowed to finish (default 300 s). If
   they do not finish, the update aborts before stopping anything.
5. **Stop** — the server is asked to shut down through
   `state/shutdown.request` (the current build watches it). The previous build
   has no watcher, so the updater terminates exactly the process whose image
   path is this installation's `workspace.exe` after a grace period.
6. **Backup** — `backups/release-<unix time>/` receives the Windows binary,
   `config.toml`, a consistent SQLite snapshot (`VACUUM INTO`, including WAL),
   `installed.json`, the deployment scripts and the four WSL components
   (`workspace-helper`, `workspace-egress`, `alxprgs-quota`, the sudoers rule)
   with their owner, mode and SHA-256.
7. **Runtime components** — helper, egress and quota helper are uploaded and
   installed atomically (`install` with explicit owner/mode), the sudoers rule is
   regenerated from the configured runtime user and quota path, `visudo -c` is
   run and the helper must answer a framed `ping`.
8. **Database** — `storage::initialize` migrates the schema inside a SQLite
   transaction after an automatic pre-migration snapshot. The instance ID is
   compared before and after, and the resulting schema must equal the release
   manifest's `schema_version`.
9. **Windows binary** — the verified `workspace.exe` is installed with a
   rename-based swap; the old file stays in the backup.
10. **Restart** — the server is started detached with the same logs, pid file
    and environment shape used by the deployment script.
11. **Verification** — `/health` must report the expected version, build SHA and
    schema. Runtime readiness and `doctor --quick` are compared with the
    pre-update state; a runtime that was ready before must be ready after.
12. **Commit** — `state/installed.json` records the new version, build SHA,
    package hash and database hash.

The server cannot overwrite its own running executable on Windows. The launcher
therefore starts the *new* binary from the staging directory as a detached
updater and keeps a pipe open; EOF on that pipe proves the old image is no longer
locked.

## 4. Rollback

Automatic rollback runs when any step after the first change fails:

- the new server is stopped;
- a copy of the post-failure database is kept at
  `backups/release-*/post-update.db` (so nothing is silently destroyed);
- the pre-update database snapshot, Windows binary, WSL components and mounted
  deployment files are restored;
- the previous server is started and must report the previous version and a
  healthy state;
- the journal ends as `rolled_back`, or as `rollback_failed` with a non-zero
  exit status if the restore itself failed.

Manual rollback uses the newest matching `backups/release-*`. A backup matches
when it produced the installed release, or when it belongs to an interrupted
update that started from the installed release (recovery).

```powershell
workspace update --status
workspace update --rollback --yes
```

If the database changed after the update, the command refuses to run until
`--allow-data-restore` is passed. A safety copy of the current database is always
written to `backups/release-*/rollback-<unix time>/workspace.db` before the
restore. Workspace files and published artifacts are never deleted by an update
or a rollback.

## 5. Journal and state

| Path | Meaning |
|---|---|
| `state/update/journal.json` | Current/last operation, stages, progress, errors |
| `state/update/journal.json.prev` | Previous valid journal (crash recovery) |
| `state/update/update.log` | Append-only journal log |
| `state/update/apply.log` | Detached updater stdout/stderr |
| `state/update/audit.log` | Administrator actions (subject, action, outcome) |
| `state/update/last-check.json` | Cached result of the last check |
| `state/installed.json` | Installed version, build SHA, package/database hashes |
| `state/secrets.dat` | DPAPI-protected secrets for this Windows user |

A journal whose process is gone and whose heartbeat is older than ten minutes is
reported as `interrupted`, both by `workspace update --status` and by the web
interface. Nothing is claimed to be running that is not.

## 6. Web interface

Administrators get an **Updates** section:

- installed version, build SHA, schema and installation directory;
- available version, channel, changelog, compatibility and last check time;
- check, install and rollback buttons with explicit confirmation;
- live stage/progress from the same journal, surviving the server restart;
- reconnect banner while the server is down, automatic recovery afterwards;
- warning and a mandatory checkbox when active jobs or a database restore are
  involved.

The HTTP API is mounted on the authenticated router and every handler checks the
administrator role on the server. GET requests are read-only; POST requests are
protected by the existing session, CSRF token and Origin checks. No request may
name a command, a URL, a package path or a backup path.

## 7. Common failures

| Symptom | Cause / action |
|---|---|
| `release_untrusted` | The manifest signature does not match `deploy/release-public-key.txt` (or `update.public_key_path`). Use a package built by the project. |
| `release_corrupt` | A file hash or size differs from the signed manifest. Re-download the package. |
| `secrets_unavailable` | Neither the environment nor `state/secrets.dat` contains both secrets. Start the server once with them, or export them in the updating shell. |
| `wsl_unavailable` | The configured WSL distribution is not installed or does not answer. Nothing was changed. |
| `incompatible` | Wrong platform/runtime, database schema outside the supported range, or an attempted downgrade. |
| `update_in_progress` | Another update holds the lock. Check `workspace update --status`. |
| `health_check_failed` | The new server did not report the expected version/schema; the update was rolled back. Read `state/update/apply.log` and `logs/server.err.log`. |
| `rollback_failed` | Automatic restore failed; the journal lists the failed component. Do not start the server blindly; inspect `backups/release-*` and the journal. |

Exit codes: `0` success, `2` operation refused or no update, `1` unexpected
failure.

## 8. Limitations

- The full Windows + WSL transaction is implemented and tested on Windows with a
  `wsl2` runtime. On a Linux host the updater replaces the binary and database
  but has no WSL components to manage.
- The running server's update API needs ALXPRGS SSO to be configured: without a
  session there is no administrator, and the API fails closed. The CLI remains
  available to the local operator.
- Staging and backups require roughly twice the package size plus three times
  the database size of free space.
- The DPAPI store is per Windows user. Running the service under a different
  account requires that account to start the server once with the environment
  variables set.
