-- Stage 009: workspaces, jobs and the audit trail.
--
-- Quotas are enforced by the filesystem; this schema stores the project
-- identifier, limits and lifecycle state needed to apply and verify them.

CREATE TABLE workspaces (
    id                  TEXT PRIMARY KEY,
    owner_id            TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name                TEXT NOT NULL,
    execution_mode      TEXT NOT NULL CHECK (execution_mode IN ('ephemeral', 'persistent')),
    state               TEXT NOT NULL CHECK (state IN (
                            'created', 'preparing', 'starting', 'running', 'idle',
                            'stopping', 'stopped', 'failed', 'deleting', 'expired')),
    capabilities        TEXT NOT NULL,
    image               TEXT NOT NULL,
    network_mode        TEXT NOT NULL CHECK (network_mode IN ('none', 'allowlist')),
    cpu_millis          INTEGER NOT NULL,
    memory_mib          INTEGER NOT NULL,
    pids                INTEGER NOT NULL,
    disk_mib            INTEGER NOT NULL,
    files               INTEGER NOT NULL,
    max_job_seconds     INTEGER NOT NULL,
    project_id          INTEGER NOT NULL UNIQUE,
    container_name      TEXT,
    created_at_unix     INTEGER NOT NULL,
    updated_at_unix     INTEGER NOT NULL,
    last_activity_unix  INTEGER NOT NULL,
    ttl_seconds         INTEGER NOT NULL,
    expires_at_unix     INTEGER NOT NULL,
    pinned              INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
    error               TEXT,
    CHECK (disk_mib > 0 AND files > 0 AND cpu_millis > 0 AND memory_mib > 0 AND pids > 0)
);

CREATE INDEX workspaces_owner ON workspaces (owner_id, created_at_unix);
CREATE INDEX workspaces_state ON workspaces (state);
CREATE INDEX workspaces_expiry ON workspaces (expires_at_unix) WHERE pinned = 0;

CREATE TABLE jobs (
    id                  TEXT PRIMARY KEY,
    workspace_id        TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    owner_id            TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    state               TEXT NOT NULL CHECK (state IN (
                            'queued', 'running', 'succeeded', 'failed',
                            'cancelled', 'timed_out', 'interrupted')),
    argv                TEXT NOT NULL,
    cwd                 TEXT NOT NULL DEFAULT '',
    env                 TEXT NOT NULL DEFAULT '{}',
    exit_code           INTEGER,
    created_at_unix     INTEGER NOT NULL,
    started_at_unix     INTEGER,
    finished_at_unix    INTEGER,
    timeout_seconds     INTEGER NOT NULL,
    error               TEXT,
    stdout              BLOB NOT NULL DEFAULT x'',
    stderr              BLOB NOT NULL DEFAULT x'',
    stdout_bytes        INTEGER NOT NULL DEFAULT 0,
    stderr_bytes        INTEGER NOT NULL DEFAULT 0,
    stdout_truncated    INTEGER NOT NULL DEFAULT 0 CHECK (stdout_truncated IN (0, 1)),
    stderr_truncated    INTEGER NOT NULL DEFAULT 0 CHECK (stderr_truncated IN (0, 1)),
    container_name      TEXT,
    idempotency_key     TEXT,
    cancel_requested    INTEGER NOT NULL DEFAULT 0 CHECK (cancel_requested IN (0, 1))
);

CREATE INDEX jobs_workspace ON jobs (workspace_id, created_at_unix);
CREATE INDEX jobs_state ON jobs (state);
CREATE UNIQUE INDEX jobs_idempotency
    ON jobs (workspace_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

CREATE TABLE audit_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    at_unix     INTEGER NOT NULL,
    actor       TEXT NOT NULL,
    action      TEXT NOT NULL,
    target      TEXT,
    outcome     TEXT NOT NULL,
    detail      TEXT
);

CREATE INDEX audit_events_at ON audit_events (at_unix DESC);

CREATE TABLE settings (
    key     TEXT PRIMARY KEY,
    value   TEXT NOT NULL
);

PRAGMA user_version = 5;
