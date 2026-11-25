CREATE TABLE instances (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    instance_id TEXT NOT NULL UNIQUE CHECK (length(instance_id) = 36),
    created_at_unix INTEGER NOT NULL CHECK (created_at_unix > 0)
) STRICT;

PRAGMA application_id = 0x414C5857;
PRAGMA user_version = 1;
