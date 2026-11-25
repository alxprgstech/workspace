use crate::{
    sso::{self, LoginResult},
    storage,
};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde::Serialize;
use std::path::Path;
use thiserror::Error;
use uuid::Uuid;
use workspace_core::config::SsoConfig;

#[derive(Debug, Error)]
pub enum UserError {
    #[error("invalid SSO identity")]
    InvalidIdentity,

    #[error("invalid local user role")]
    InvalidRole,

    #[error("user storage error: {0}")]
    Storage(#[from] storage::StorageError),

    #[error("user database operation failed")]
    Database(#[from] rusqlite::Error),
}

#[derive(Debug, Serialize)]
pub struct UserRecord {
    pub id: String,
    pub issuer: String,
    pub subject: String,
    pub username: Option<String>,
    pub role: String,
    pub is_enabled: bool,
}

pub(crate) fn read_user(row: &Row<'_>) -> rusqlite::Result<UserRecord> {
    Ok(UserRecord {
        id: row.get(0)?,
        issuer: row.get(1)?,
        subject: row.get(2)?,
        username: row.get(3)?,
        role: row.get(4)?,
        is_enabled: row.get(5)?,
    })
}

fn validate_subject(config: &SsoConfig, subject: &str) -> Result<(), UserError> {
    sso::validate_config(config).map_err(|_| UserError::InvalidIdentity)?;

    let uuid = Uuid::parse_str(subject).map_err(|_| UserError::InvalidIdentity)?;

    if uuid.is_nil() || uuid.to_string() != subject {
        return Err(UserError::InvalidIdentity);
    }

    Ok(())
}

pub fn list_users(data_dir: &Path, config: &SsoConfig) -> Result<Vec<UserRecord>, UserError> {
    sso::validate_config(config).map_err(|_| UserError::InvalidIdentity)?;

    let connection = storage::open(data_dir)?;

    let mut statement = connection.prepare(
        "SELECT id, issuer, subject, username, role, is_enabled
         FROM users
         WHERE issuer = ?1
         ORDER BY created_at_unix, id",
    )?;

    let rows = statement.query_map(params![&config.issuer], read_user)?;

    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn grant_access(
    data_dir: &Path,
    config: &SsoConfig,
    subject: &str,
    role: &str,
) -> Result<(), UserError> {
    validate_subject(config, subject)?;

    if !matches!(role, "user" | "admin") {
        return Err(UserError::InvalidRole);
    }

    let mut connection = storage::open(data_dir)?;

    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

    tx.execute(
        "INSERT INTO users (
            id, issuer, subject, role, is_enabled,
            created_at_unix, updated_at_unix
         ) VALUES (
            ?1, ?2, ?3, ?4, 1, unixepoch(), unixepoch()
         )
         ON CONFLICT(issuer, subject) DO UPDATE SET
            role = excluded.role,
            is_enabled = 1,
            updated_at_unix = unixepoch()",
        params![Uuid::new_v4().to_string(), &config.issuer, subject, role,],
    )?;

    tx.execute(
        "DELETE FROM web_sessions
         WHERE user_id IN (
            SELECT id FROM users
            WHERE issuer = ?1 AND subject = ?2
         )",
        params![&config.issuer, subject],
    )?;

    tx.execute(
        "UPDATE mcp_oauth_grants
         SET revoked_at_unix = unixepoch()
         WHERE revoked_at_unix IS NULL
           AND user_id IN (
               SELECT id FROM users
               WHERE issuer = ?1 AND subject = ?2
           )",
        params![&config.issuer, subject],
    )?;

    tx.commit()?;

    Ok(())
}

pub fn disable_access(
    data_dir: &Path,
    config: &SsoConfig,
    subject: &str,
) -> Result<bool, UserError> {
    validate_subject(config, subject)?;

    let mut connection = storage::open(data_dir)?;

    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

    let changed = tx.execute(
        "UPDATE users
         SET is_enabled = 0,
             updated_at_unix = unixepoch()
         WHERE issuer = ?1 AND subject = ?2",
        params![&config.issuer, subject],
    )?;

    tx.execute(
        "DELETE FROM web_sessions
         WHERE user_id IN (
            SELECT id FROM users
            WHERE issuer = ?1 AND subject = ?2
         )",
        params![&config.issuer, subject],
    )?;

    tx.execute(
        "UPDATE mcp_oauth_grants
         SET revoked_at_unix = unixepoch()
         WHERE revoked_at_unix IS NULL
           AND user_id IN (
               SELECT id FROM users
               WHERE issuer = ?1 AND subject = ?2
           )",
        params![&config.issuer, subject],
    )?;

    tx.commit()?;

    Ok(changed == 1)
}

pub(crate) fn record_login(
    connection: &Connection,
    config: &SsoConfig,
    login: &LoginResult,
) -> Result<UserRecord, UserError> {
    validate_subject(config, &login.identity.subject)?;

    if login.identity.issuer != config.issuer {
        return Err(UserError::InvalidIdentity);
    }

    connection.execute(
        "INSERT INTO users (
            id, issuer, subject, username, email,
            email_verified, created_at_unix, updated_at_unix
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6,
            unixepoch(), unixepoch()
         )
         ON CONFLICT(issuer, subject) DO UPDATE SET
            username = excluded.username,
            email = excluded.email,
            email_verified = excluded.email_verified,
            updated_at_unix = unixepoch()",
        params![
            Uuid::new_v4().to_string(),
            &config.issuer,
            &login.identity.subject,
            &login.username,
            &login.email,
            login.email_verified,
        ],
    )?;

    let user = connection
        .query_row(
            "SELECT id, issuer, subject, username, role, is_enabled
         FROM users
         WHERE issuer = ?1 AND subject = ?2",
            params![&config.issuer, &login.identity.subject],
            read_user,
        )
        .optional()?;

    user.ok_or(UserError::InvalidIdentity)
}
