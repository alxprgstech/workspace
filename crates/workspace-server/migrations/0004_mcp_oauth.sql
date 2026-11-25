CREATE TABLE mcp_oauth_clients (
    client_id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    secret_hash BLOB NOT NULL
        CHECK (length(secret_hash) = 32),
    redirect_uri TEXT NOT NULL,
    allowed_scope TEXT NOT NULL,
    is_enabled INTEGER NOT NULL DEFAULT 1
        CHECK (is_enabled IN (0, 1)),
    created_at_unix INTEGER NOT NULL
) STRICT;

CREATE TABLE mcp_oauth_requests (
    handle_hash BLOB PRIMARY KEY NOT NULL
        CHECK (length(handle_hash) = 32),
    client_id TEXT NOT NULL
        REFERENCES mcp_oauth_clients(client_id) ON DELETE CASCADE,
    encrypted_payload BLOB NOT NULL,
    created_at_unix INTEGER NOT NULL,
    expires_at_unix INTEGER NOT NULL,
    CHECK (expires_at_unix > created_at_unix)
) STRICT;

CREATE TABLE mcp_oauth_grants (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL
        REFERENCES users(id) ON DELETE CASCADE,
    client_id TEXT NOT NULL
        REFERENCES mcp_oauth_clients(client_id) ON DELETE CASCADE,
    resource TEXT NOT NULL,
    scope TEXT NOT NULL,
    created_at_unix INTEGER NOT NULL,
    expires_at_unix INTEGER NOT NULL,
    revoked_at_unix INTEGER,
    CHECK (expires_at_unix > created_at_unix)
) STRICT;

CREATE TABLE mcp_oauth_codes (
    code_hash BLOB PRIMARY KEY NOT NULL
        CHECK (length(code_hash) = 32),
    grant_id TEXT NOT NULL
        REFERENCES mcp_oauth_grants(id) ON DELETE CASCADE,
    redirect_uri TEXT NOT NULL,
    code_challenge TEXT NOT NULL
        CHECK (length(code_challenge) = 43),
    created_at_unix INTEGER NOT NULL,
    expires_at_unix INTEGER NOT NULL,
    consumed_at_unix INTEGER,
    CHECK (expires_at_unix > created_at_unix)
) STRICT;

CREATE TABLE mcp_access_tokens (
    token_hash BLOB PRIMARY KEY NOT NULL
        CHECK (length(token_hash) = 32),
    grant_id TEXT NOT NULL
        REFERENCES mcp_oauth_grants(id) ON DELETE CASCADE,
    created_at_unix INTEGER NOT NULL,
    expires_at_unix INTEGER NOT NULL,
    revoked_at_unix INTEGER,
    CHECK (expires_at_unix > created_at_unix)
) STRICT;

CREATE TABLE mcp_refresh_tokens (
    token_hash BLOB PRIMARY KEY NOT NULL
        CHECK (length(token_hash) = 32),
    grant_id TEXT NOT NULL
        REFERENCES mcp_oauth_grants(id) ON DELETE CASCADE,
    generation INTEGER NOT NULL
        CHECK (generation >= 0),
    created_at_unix INTEGER NOT NULL,
    expires_at_unix INTEGER NOT NULL,
    consumed_at_unix INTEGER,
    revoked_at_unix INTEGER,
    CHECK (expires_at_unix > created_at_unix),
    UNIQUE (grant_id, generation)
) STRICT;

CREATE INDEX mcp_requests_expiry_idx
    ON mcp_oauth_requests(expires_at_unix);

CREATE INDEX mcp_requests_client_idx
    ON mcp_oauth_requests(client_id);

CREATE INDEX mcp_grants_user_idx
    ON mcp_oauth_grants(user_id);

CREATE INDEX mcp_grants_client_idx
    ON mcp_oauth_grants(client_id);

CREATE INDEX mcp_grants_expiry_idx
    ON mcp_oauth_grants(expires_at_unix);

CREATE INDEX mcp_codes_grant_idx
    ON mcp_oauth_codes(grant_id);

CREATE INDEX mcp_codes_expiry_idx
    ON mcp_oauth_codes(expires_at_unix);

CREATE INDEX mcp_access_grant_idx
    ON mcp_access_tokens(grant_id);

CREATE INDEX mcp_access_expiry_idx
    ON mcp_access_tokens(expires_at_unix);

CREATE INDEX mcp_refresh_expiry_idx
    ON mcp_refresh_tokens(expires_at_unix);

PRAGMA user_version = 4;
