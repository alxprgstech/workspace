//! Test-only helper: create a schema-9 Workspace database with real rows.
//!
//! Used by the update acceptance lab to prove the 9 → 11 migration performed by
//! the updater, with preserved users, instance ID and settings. Migrations are
//! applied exactly as they are shipped; only the version stops at 9.

use rusqlite::{Connection, params};
use std::path::PathBuf;
use workspace_core::config::Config;

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_initial.sql"),
    include_str!("../migrations/0002_auth.sql"),
    include_str!("../migrations/0003_refresh_lease.sql"),
    include_str!("../migrations/0004_mcp_oauth.sql"),
    include_str!("../migrations/0005_workspaces.sql"),
    include_str!("../migrations/0006_scopes.sql"),
    include_str!("../migrations/0007_artifacts.sql"),
    include_str!("../migrations/0008_images.sql"),
    include_str!("../migrations/0009_scopes_expand.sql"),
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config_path = std::env::args().nth(1).ok_or("config path is required")?;
    let config = Config::load(&config_path)?;
    let sso = config.sso.as_ref().ok_or("SSO must be configured")?;

    let database: PathBuf = config.storage.data_dir.join("state").join("workspace.db");

    if database.exists() {
        return Err(format!("{} already exists", database.display()).into());
    }

    std::fs::create_dir_all(database.parent().ok_or("invalid database path")?)?;

    let connection = Connection::open(&database)?;
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;",
    )?;

    for migration in MIGRATIONS {
        connection.execute_batch(migration)?;
    }

    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    if version != 9 {
        return Err(format!("expected schema 9, got {version}").into());
    }

    let instance_id = "0b7c4d1e-2f3a-4b5c-8d9e-0f1a2b3c4d5e";
    let user_id = "1c8d5e2f-3a4b-4c5d-9e6f-1a2b3c4d5e6f";
    let workspace_id = "2d9e6f3a-4b5c-4d6e-8f7a-2b3c4d5e6f70";
    let subject = "3eaf7a4b-5c6d-4e7f-9a8b-3c4d5e6f7081";

    connection.execute(
        "INSERT INTO instances (singleton, instance_id, created_at_unix)
         VALUES (1, ?1, unixepoch())",
        params![instance_id],
    )?;

    connection.execute(
        "INSERT INTO users (
            id, issuer, subject, username, email, email_verified, role,
            is_enabled, created_at_unix, updated_at_unix
         ) VALUES (?1, ?2, ?3, 'Preserved Admin', 'lab@example.invalid', 1, 'admin', 1, unixepoch(), unixepoch())",
        params![user_id, &sso.issuer, subject],
    )?;

    connection.execute(
        "INSERT INTO settings (key, value)
         VALUES ('update.lab.sentinel', 'preserved-across-migration')",
        [],
    )?;

    connection.execute(
        "INSERT INTO workspaces (
            id, owner_id, name, execution_mode, state, capabilities, image,
            network_mode, cpu_millis, memory_mib, pids, disk_mib, files,
            max_job_seconds, project_id, created_at_unix, updated_at_unix,
            last_activity_unix, ttl_seconds, expires_at_unix, pinned
         ) VALUES (
            ?1, ?2, 'preserved-workspace', 'ephemeral', 'stopped', '[]',
            'debian@sha256:0000000000000000000000000000000000000000000000000000000000000000',
            'none', 500, 512, 64, 1024, 10000, 300, 9001, unixepoch(), unixepoch(),
            unixepoch(), 86400, unixepoch() + 86400, 1
         )",
        params![workspace_id, user_id],
    )?;

    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;

    println!(
        "{}",
        serde_json::json!({
            "database": database.display().to_string(),
            "schema_version": version,
            "instance_id": instance_id,
            "user_id": user_id,
            "subject": subject,
            "workspace_id": workspace_id,
        })
    );

    Ok(())
}
