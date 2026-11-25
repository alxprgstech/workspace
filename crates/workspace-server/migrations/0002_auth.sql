CREATE TABLE users (
    id TEXT PRIMARY KEY NOT NULL,
    issuer TEXT NOT NULL,
    subject TEXT NOT NULL,
    username TEXT,
    email TEXT,
    email_verified INTEGER NOT NULL DEFAULT 0
        CHECK (email_verified IN (0, 1)),
    role TEXT NOT NULL DEFAULT 'user'
        CHECK (role IN ('user', 'admin')),
    is_enabled INTEGER NOT NULL DEFAULT 0
        CHECK (is_enabled IN (0, 1)),
    created_at_unix INTEGER NOT NULL,
    updated_at_unix INTEGER NOT NULL,

    UNIQUE (issuer, subject)
) STRICT;

CREATE TABLE auth_flows (
    handle_hash BLOB PRIMARY KEY NOT NULL
        CHECK (length(handle_hash) = 32),
    state_hash BLOB NOT NULL UNIQUE
        CHECK (length(state_hash) = 32),
    client_id TEXT NOT NULL,
    redirect_uri TEXT NOT NULL,
    encrypted_secrets BLOB NOT NULL,
    created_at_unix INTEGER NOT NULL,
    expires_at_unix INTEGER NOT NULL,

    CHECK (expires_at_unix > created_at_unix)
) STRICT;

CREATE INDEX auth_flows_expiry_idx
    ON auth_flows (expires_at_unix);

CREATE TABLE web_sessions (
    handle_hash BLOB PRIMARY KEY NOT NULL
        CHECK (length(handle_hash) = 32),
    user_id TEXT NOT NULL
        REFERENCES users(id) ON DELETE CASCADE,
    client_id TEXT NOT NULL,
    csrf_hash BLOB NOT NULL
        CHECK (length(csrf_hash) = 32),
    encrypted_tokens BLOB NOT NULL,
    created_at_unix INTEGER NOT NULL,
    expires_at_unix INTEGER NOT NULL,
    access_expires_at_unix INTEGER NOT NULL,
    refresh_started_at_unix INTEGER,
    revoked_at_unix INTEGER,
    generation INTEGER NOT NULL DEFAULT 0
        CHECK (generation >= 0),

    CHECK (expires_at_unix > created_at_unix),
    CHECK (access_expires_at_unix <= expires_at_unix)
) STRICT;

CREATE INDEX web_sessions_user_idx
    ON web_sessions (user_id);

CREATE INDEX web_sessions_expiry_idx
    ON web_sessions (expires_at_unix);

PRAGMA user_version = 2;
