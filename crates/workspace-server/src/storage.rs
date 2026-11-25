use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use serde::Serialize;
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};
use thiserror::Error;
use uuid::Uuid;

pub const SCHEMA_VERSION: u32 = 3;

const APPLICATION_ID: i64 = 0x414C5857;
const MIN_SQLITE_VERSION: i32 = 3_051_003;

const MIGRATION_001: &str = include_str!("../migrations/0001_initial.sql");
const MIGRATION_002: &str = include_str!("../migrations/0002_auth.sql");
const MIGRATION_003: &str = include_str!("../migrations/0003_refresh_lease.sql");

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StorageInfo {
    pub instance_id: String,
    pub schema_version: u32,
    pub sqlite_version: String,
    pub journal_mode: String,
    pub created_at_unix: i64,
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("storage filesystem error: {0}")]
    Io(#[from] io::Error),

    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("unsupported database schema version: {0}")]
    UnsupportedSchema(u32),

    #[error("unexpected database application ID")]
    ForeignDatabase,

    #[error("invalid instance identifier")]
    InvalidInstance,

    #[error("WAL mode is unavailable")]
    WalUnavailable,

    #[error("database integrity check failed: {0}")]
    Integrity(String),

    #[error("SQLite version is too old: {0}")]
    UnsupportedSqlite(String),
}

pub fn database_path(data_dir: &Path) -> PathBuf {
    data_dir.join("state").join("workspace.db")
}

fn check_sqlite_version() -> Result<(), StorageError> {
    if rusqlite::version_number() < MIN_SQLITE_VERSION {
        return Err(StorageError::UnsupportedSqlite(
            rusqlite::version().to_owned(),
        ));
    }

    Ok(())
}

fn configure(connection: &Connection) -> Result<(), StorageError> {
    connection.busy_timeout(Duration::from_secs(5))?;

    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA synchronous = FULL;
         PRAGMA trusted_schema = OFF;",
    )?;

    Ok(())
}

fn schema_version(connection: &Connection) -> Result<u32, StorageError> {
    Ok(connection.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn application_id(connection: &Connection) -> Result<i64, StorageError> {
    Ok(connection.query_row("PRAGMA application_id", [], |row| row.get(0))?)
}

fn migrate(connection: &mut Connection) -> Result<(), StorageError> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

    let version = schema_version(&tx)?;
    let app_id = application_id(&tx)?;

    if version == 0 {
        let objects: i64 = tx.query_row(
            "SELECT count(*) FROM sqlite_schema
             WHERE name NOT GLOB 'sqlite_*'",
            [],
            |row| row.get(0),
        )?;

        if app_id != 0 || objects != 0 {
            return Err(StorageError::ForeignDatabase);
        }

        tx.execute_batch(MIGRATION_001)?;

        tx.execute(
            "INSERT INTO instances
             (singleton, instance_id, created_at_unix)
             VALUES (1, ?1, unixepoch())",
            params![Uuid::new_v4().to_string()],
        )?;
    } else {
        if app_id != APPLICATION_ID {
            return Err(StorageError::ForeignDatabase);
        }

        if version > SCHEMA_VERSION {
            return Err(StorageError::UnsupportedSchema(version));
        }
    }

    if version < 2 {
        tx.execute_batch(MIGRATION_002)?;
    }

    if version < 3 {
        tx.execute_batch(MIGRATION_003)?;
    }

    tx.commit()?;

    Ok(())
}

fn read_info(connection: &Connection) -> Result<StorageInfo, StorageError> {
    if application_id(connection)? != APPLICATION_ID {
        return Err(StorageError::ForeignDatabase);
    }

    let version = schema_version(connection)?;

    if version != SCHEMA_VERSION {
        return Err(StorageError::UnsupportedSchema(version));
    }

    let journal_mode: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;

    if journal_mode != "wal" {
        return Err(StorageError::WalUnavailable);
    }

    let (instance_id, created_at_unix): (String, i64) = connection.query_row(
        "SELECT instance_id, created_at_unix
             FROM instances WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    let uuid = Uuid::parse_str(&instance_id).map_err(|_| StorageError::InvalidInstance)?;

    if uuid.is_nil() {
        return Err(StorageError::InvalidInstance);
    }

    Ok(StorageInfo {
        instance_id,
        schema_version: version,
        sqlite_version: rusqlite::version().to_owned(),
        journal_mode,
        created_at_unix,
    })
}

pub fn initialize(data_dir: &Path) -> Result<StorageInfo, StorageError> {
    check_sqlite_version()?;

    fs::create_dir_all(data_dir.join("state"))?;

    let path = database_path(data_dir);
    let mut connection = Connection::open(path)?;

    configure(&connection)?;
    migrate(&mut connection)?;

    let journal_mode: String =
        connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;

    if journal_mode != "wal" {
        return Err(StorageError::WalUnavailable);
    }

    read_info(&connection)
}

pub(crate) fn open(data_dir: &Path) -> Result<Connection, StorageError> {
    check_sqlite_version()?;

    let connection =
        Connection::open_with_flags(database_path(data_dir), OpenFlags::SQLITE_OPEN_READ_WRITE)?;

    configure(&connection)?;
    read_info(&connection)?;

    Ok(connection)
}

pub fn inspect(data_dir: &Path) -> Result<Option<StorageInfo>, StorageError> {
    check_sqlite_version()?;

    let path = database_path(data_dir);

    if !path.try_exists()? {
        return Ok(None);
    }

    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;

    connection.busy_timeout(Duration::from_secs(5))?;

    let info = read_info(&connection)?;

    let integrity: String = connection.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;

    if integrity != "ok" {
        return Err(StorageError::Integrity(integrity));
    }

    Ok(Some(info))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initializes_database() {
        let dir = tempfile::tempdir().unwrap();
        let info = initialize(dir.path()).unwrap();

        assert_eq!(info.schema_version, SCHEMA_VERSION);
        assert_eq!(info.journal_mode, "wal");
        assert!(Uuid::parse_str(&info.instance_id).is_ok());
        assert!(database_path(dir.path()).is_file());
    }

    #[test]
    fn preserves_instance_id_on_reopen() {
        let dir = tempfile::tempdir().unwrap();

        let first = initialize(dir.path()).unwrap();
        let second = initialize(dir.path()).unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn independent_instances_have_unique_ids() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();

        let a = initialize(first.path()).unwrap();
        let b = initialize(second.path()).unwrap();

        assert_ne!(a.instance_id, b.instance_id);
    }

    #[test]
    fn inspect_does_not_initialize_database() {
        let dir = tempfile::tempdir().unwrap();

        assert!(inspect(dir.path()).unwrap().is_none());
        assert!(!database_path(dir.path()).exists());
    }

    #[test]
    fn inspect_reads_initialized_database() {
        let dir = tempfile::tempdir().unwrap();

        let expected = initialize(dir.path()).unwrap();
        let actual = inspect(dir.path()).unwrap().unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn rejects_foreign_database() {
        let dir = tempfile::tempdir().unwrap();

        fs::create_dir_all(dir.path().join("state")).unwrap();

        let connection = Connection::open(database_path(dir.path())).unwrap();

        connection
            .execute_batch("CREATE TABLE foreign_data (id INTEGER PRIMARY KEY);")
            .unwrap();

        drop(connection);

        assert!(matches!(
            initialize(dir.path()),
            Err(StorageError::ForeignDatabase)
        ));
    }

    #[test]
    fn rejects_newer_schema() {
        let dir = tempfile::tempdir().unwrap();

        initialize(dir.path()).unwrap();

        let connection = Connection::open(database_path(dir.path())).unwrap();

        connection
            .execute_batch("PRAGMA user_version = 999;")
            .unwrap();

        drop(connection);

        assert!(matches!(
            initialize(dir.path()),
            Err(StorageError::UnsupportedSchema(999))
        ));
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    const INSTANCE_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn create_v1(data_dir: &Path) -> Connection {
        fs::create_dir_all(data_dir.join("state")).unwrap();

        let connection = Connection::open(database_path(data_dir)).unwrap();

        configure(&connection).unwrap();
        connection.execute_batch(MIGRATION_001).unwrap();

        connection
            .execute(
                "INSERT INTO instances
             (singleton, instance_id, created_at_unix)
             VALUES (1, ?1, unixepoch())",
                params![INSTANCE_ID],
            )
            .unwrap();

        connection
    }

    #[test]
    fn upgrades_v1_preserving_instance_identity() {
        let dir = tempfile::tempdir().unwrap();

        drop(create_v1(dir.path()));

        let info = initialize(dir.path()).unwrap();

        assert_eq!(info.schema_version, SCHEMA_VERSION);
        assert_eq!(info.instance_id, INSTANCE_ID);

        let connection = open(dir.path()).unwrap();

        let count: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema
             WHERE type = 'table'
             AND name IN ('users', 'auth_flows', 'web_sessions')",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(count, 3);
    }

    fn create_v2(data_dir: &Path) -> Connection {
        let connection = create_v1(data_dir);

        connection.execute_batch(MIGRATION_002).unwrap();

        connection
    }

    #[test]
    fn upgrades_v2_without_losing_auth_rows() {
        let dir = tempfile::tempdir().unwrap();

        let connection = create_v2(dir.path());

        connection
            .execute_batch(
                "INSERT INTO users
                 (id, issuer, subject, role, is_enabled, created_at_unix, updated_at_unix)
                 VALUES ('user-1', 'https://auth.example.com', 'subject-1', 'admin', 1, 1, 1);",
            )
            .unwrap();

        drop(connection);

        let info = initialize(dir.path()).unwrap();

        assert_eq!(info.schema_version, SCHEMA_VERSION);
        assert_eq!(info.instance_id, INSTANCE_ID);

        let connection = open(dir.path()).unwrap();

        let users: i64 = connection
            .query_row("SELECT count(*) FROM users", [], |row| row.get(0))
            .unwrap();

        assert_eq!(users, 1);

        let lease_columns: i64 = connection
            .query_row(
                "SELECT count(*) FROM pragma_table_info('web_sessions')
                 WHERE name = 'refresh_lease_id'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(lease_columns, 1);

        // The lease identifier is unique while it exists and is free when NULL.
        connection
            .execute_batch(
                "INSERT INTO web_sessions
                 (handle_hash, user_id, client_id, csrf_hash, encrypted_tokens,
                  created_at_unix, expires_at_unix, access_expires_at_unix)
                 VALUES (randomblob(32), 'user-1', 'c', randomblob(32), x'03', 1, 100, 10),
                        (randomblob(32), 'user-1', 'c', randomblob(32), x'04', 1, 100, 10);

                 UPDATE web_sessions
                 SET refresh_lease_id = x'00000000000000000000000000000001'
                 WHERE rowid = 1;",
            )
            .unwrap();

        // A lease identifier can only belong to one session at a time.
        let duplicate = connection.execute(
            "UPDATE web_sessions
             SET refresh_lease_id = x'00000000000000000000000000000001'
             WHERE rowid = 2",
            [],
        );

        assert!(duplicate.is_err());

        // Released leases are free again, so many rows can hold NULL.
        connection
            .execute_batch("UPDATE web_sessions SET refresh_lease_id = NULL;")
            .unwrap();
    }

    #[test]
    fn failed_migration_rolls_back_schema_changes() {
        let dir = tempfile::tempdir().unwrap();

        let connection = create_v1(dir.path());

        connection
            .execute_batch(
                "CREATE TABLE web_sessions (
                id INTEGER PRIMARY KEY
            );",
            )
            .unwrap();

        drop(connection);

        assert!(initialize(dir.path()).is_err());

        let connection = Connection::open(database_path(dir.path())).unwrap();

        assert_eq!(schema_version(&connection).unwrap(), 1);

        let count: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema
             WHERE name IN ('users', 'auth_flows')",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(count, 0);
    }
}
