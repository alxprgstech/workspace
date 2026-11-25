-- Stage 012: published artifacts and their expiring download links.

CREATE TABLE artifacts (
    id                TEXT PRIMARY KEY,
    owner_id          TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    workspace_id      TEXT,
    display_name      TEXT NOT NULL,
    stored_name       TEXT NOT NULL,
    media_type        TEXT NOT NULL,
    size_bytes        INTEGER NOT NULL CHECK (size_bytes >= 0),
    sha256            TEXT NOT NULL,
    created_at_unix   INTEGER NOT NULL,
    expires_at_unix   INTEGER NOT NULL,
    revoked_at_unix   INTEGER
);

CREATE INDEX artifacts_owner ON artifacts (owner_id, created_at_unix DESC);
CREATE INDEX artifacts_expiry ON artifacts (expires_at_unix);

CREATE TABLE artifact_links (
    token_hash        BLOB PRIMARY KEY,
    artifact_id       TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
    created_at_unix   INTEGER NOT NULL,
    expires_at_unix   INTEGER NOT NULL,
    revoked_at_unix   INTEGER
);

CREATE INDEX artifact_links_artifact ON artifact_links (artifact_id);

PRAGMA user_version = 7;
