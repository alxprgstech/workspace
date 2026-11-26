-- Stage B: one active job per workspace, enforced by the database.
--
-- The application already serialises submissions, but an invariant that only
-- exists in code can be violated by a second process, a crash between the
-- check and the insert, or a future code path. The unique index makes the
-- database the last authority.
--
-- A database that predates the invariant can only be here because of that
-- race, so keep the oldest active job and mark the rest interrupted instead of
-- failing the migration.

UPDATE jobs
SET state = 'interrupted',
    error = 'superseded by the single-active-job invariant',
    finished_at_unix = unixepoch()
WHERE state IN ('queued', 'running')
  AND rowid NOT IN (
      SELECT min(rowid) FROM jobs
      WHERE state IN ('queued', 'running')
      GROUP BY workspace_id
  );

CREATE UNIQUE INDEX jobs_one_active
    ON jobs (workspace_id)
    WHERE state IN ('queued', 'running');

PRAGMA user_version = 10;
