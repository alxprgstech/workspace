ALTER TABLE web_sessions ADD COLUMN refresh_lease_id BLOB
    CHECK (refresh_lease_id IS NULL OR length(refresh_lease_id) = 16);

CREATE UNIQUE INDEX web_sessions_refresh_lease_idx
    ON web_sessions (refresh_lease_id)
    WHERE refresh_lease_id IS NOT NULL;

PRAGMA user_version = 3;
