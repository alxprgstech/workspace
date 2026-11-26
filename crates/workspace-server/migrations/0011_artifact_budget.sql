-- Stage C: artifacts have a reserved budget before their bytes are copied.
--
-- A reservation is written in the same transaction that checks the owner and
-- global budgets, so two concurrent publications cannot both pass the check.
-- The row is removed when the artifact is committed or the publication fails;
-- a crash leaves it behind, and the cleanup pass removes reservations older
-- than a bounded age.

CREATE TABLE artifact_reservations (
    id                TEXT PRIMARY KEY,
    owner_id          TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    bytes             INTEGER NOT NULL CHECK (bytes >= 0),
    created_at_unix   INTEGER NOT NULL
);

CREATE INDEX artifact_reservations_age ON artifact_reservations (created_at_unix);
CREATE INDEX artifact_reservations_owner ON artifact_reservations (owner_id);

PRAGMA user_version = 11;
