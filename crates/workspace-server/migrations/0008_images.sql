-- Stage 010: trusted capability images and their cache keys.

CREATE TABLE image_specs (
    cache_key       TEXT PRIMARY KEY,
    modules         TEXT NOT NULL,
    base_image      TEXT NOT NULL,
    image_ref       TEXT,
    image_id        TEXT,
    state           TEXT NOT NULL CHECK (state IN ('building', 'ready', 'failed')),
    error           TEXT,
    created_at_unix INTEGER NOT NULL,
    updated_at_unix INTEGER NOT NULL
);

PRAGMA user_version = 8;
