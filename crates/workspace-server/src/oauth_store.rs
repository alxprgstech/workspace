#[path = "oauth_browser.rs"]
pub(crate) mod browser;

use crate::{crypto, sso, storage};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::path::{Path, PathBuf};
use thiserror::Error;
use uuid::Uuid;
use workspace_core::config::SsoConfig;

/// Scope required by tokens issued before per-operation scopes existed.
///
/// Such tokens stay read-only: they are normalised to `workspace:read` and can
/// never be silently upgraded.
pub const SCOPE: &str = "workspace.read";

/// Scopes this deployment can grant.
pub const SCOPES: &[&str] = &[
    "workspace:read",
    "workspace:write",
    "workspace:delete",
    "execution:run",
    "files:read",
    "files:write",
    "files:import",
    "artifacts:read",
    "artifacts:publish",
];

/// Scope set a newly registered client asks for.
pub fn default_scope() -> String {
    SCOPES.join(" ")
}

/// Normalise a stored scope string into canonical, sorted scopes.
///
/// Unknown values are dropped, and the legacy `workspace.read` scope maps to
/// the read-only subset.
pub fn parse_scopes(raw: &str) -> Vec<String> {
    let mut scopes: Vec<String> = raw
        .split_whitespace()
        .filter_map(|scope| match scope {
            "workspace.read" => Some("workspace:read".to_owned()),
            other if SCOPES.contains(&other) => Some(other.to_owned()),
            _ => None,
        })
        .collect();

    scopes.sort();
    scopes.dedup();
    scopes
}

/// Whether every scope in `granted` is permitted by `allowed`.
pub fn scope_subset(granted: &[String], allowed: &[String]) -> bool {
    !granted.is_empty() && granted.iter().all(|scope| allowed.contains(scope))
}
pub const CODE_TTL: i64 = 60;
pub const ACCESS_TTL: i64 = 600;
pub const GRANT_TTL: i64 = 30 * 24 * 60 * 60;

#[derive(Debug, Error)]
pub enum OAuthError {
    #[error("invalid OAuth request")]
    InvalidRequest,

    #[error("invalid OAuth client")]
    InvalidClient,

    #[error("invalid OAuth grant")]
    InvalidGrant,

    #[error("access denied")]
    AccessDenied,

    #[error("OAuth storage error")]
    Storage(#[from] storage::StorageError),

    #[error("OAuth database operation failed")]
    Database(#[from] rusqlite::Error),

    #[error("OAuth cryptographic operation failed")]
    Crypto(#[from] crypto::CryptoError),
}

pub struct OAuthStore {
    data_dir: PathBuf,
    resource: String,
    user_issuer: String,
}

#[derive(Debug)]
pub struct ClientRecord {
    pub client_id: String,
    pub name: String,
    pub redirect_uri: String,
    pub allowed_scope: String,
    pub is_enabled: bool,
}

// Secret-bearing structures intentionally do not implement Debug.
pub struct ClientCredentials {
    pub client_id: String,
    pub client_secret: String,
}

pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
    pub scope: String,
}

#[derive(Debug)]
pub struct AccessIdentity {
    pub user_id: String,
    pub subject: String,
    pub role: String,
    pub grant_id: String,
    pub scopes: Vec<String>,
}

fn random_token(prefix: &str) -> Result<String, OAuthError> {
    Ok(format!("{prefix}{}", crypto::random_handle()?))
}

fn valid_token(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 43
            && suffix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

fn valid_redirect(value: &str) -> bool {
    if value.len() > 2048 {
        return false;
    }

    openidconnect::reqwest::Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && url.query().is_none()
    })
}

fn valid_challenge(value: &str) -> bool {
    value.len() == 43
        && URL_SAFE_NO_PAD
            .decode(value)
            .is_ok_and(|bytes| bytes.len() == 32 && URL_SAFE_NO_PAD.encode(bytes) == value)
}

fn pkce_challenge(verifier: &str) -> Option<String> {
    if !(43..=128).contains(&verifier.len())
        || !verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'))
    {
        return None;
    }

    Some(URL_SAFE_NO_PAD.encode(crypto::hash_handle(verifier)))
}

fn now(connection: &Connection) -> Result<i64, OAuthError> {
    Ok(connection.query_row("SELECT unixepoch()", [], |row| row.get(0))?)
}

fn authenticate_client(
    connection: &Connection,
    client_id: &str,
    secret: &str,
) -> Result<(), OAuthError> {
    if !valid_token(client_id, "mcp_client_") || !valid_token(secret, "mcp_secret_") {
        return Err(OAuthError::InvalidClient);
    }

    let exists: bool = connection.query_row(
        "SELECT EXISTS (
            SELECT 1 FROM mcp_oauth_clients
            WHERE client_id = ?1
              AND secret_hash = ?2
              AND is_enabled = 1
        )",
        params![client_id, &crypto::hash_handle(secret)[..]],
        |row| row.get(0),
    )?;

    if !exists {
        return Err(OAuthError::InvalidClient);
    }

    Ok(())
}

fn revoke_grant(connection: &Connection, grant_id: &str) -> Result<(), OAuthError> {
    connection.execute(
        "UPDATE mcp_oauth_grants
         SET revoked_at_unix = unixepoch()
         WHERE id = ?1 AND revoked_at_unix IS NULL",
        params![grant_id],
    )?;

    Ok(())
}

fn issue_pair(
    connection: &Connection,
    grant_id: &str,
    generation: i64,
    current_time: i64,
    grant_expiry: i64,
    scope: &str,
) -> Result<TokenPair, OAuthError> {
    let access_expiry = (current_time + ACCESS_TTL).min(grant_expiry);

    if access_expiry <= current_time {
        return Err(OAuthError::InvalidGrant);
    }

    let access_token = random_token("mcp_at_")?;
    let refresh_token = random_token("mcp_rt_")?;

    connection.execute(
        "INSERT INTO mcp_access_tokens
         (token_hash, grant_id, created_at_unix, expires_at_unix)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            &crypto::hash_handle(&access_token)[..],
            grant_id,
            current_time,
            access_expiry,
        ],
    )?;

    connection.execute(
        "INSERT INTO mcp_refresh_tokens
         (token_hash, grant_id, generation,
          created_at_unix, expires_at_unix)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            &crypto::hash_handle(&refresh_token)[..],
            grant_id,
            generation,
            current_time,
            grant_expiry,
        ],
    )?;

    Ok(TokenPair {
        access_token,
        refresh_token,
        expires_in: access_expiry - current_time,
        scope: scope.to_owned(),
    })
}

impl OAuthStore {
    pub fn new(data_dir: &Path, config: &SsoConfig) -> Result<Self, OAuthError> {
        sso::validate_config(config).map_err(|_| OAuthError::InvalidRequest)?;

        let redirect = openidconnect::reqwest::Url::parse(&config.redirect_uri)
            .map_err(|_| OAuthError::InvalidRequest)?;

        let resource = redirect
            .join("/chatgpt/mcp")
            .map_err(|_| OAuthError::InvalidRequest)?;

        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            resource: resource.to_string(),
            user_issuer: config.issuer.clone(),
        })
    }

    pub fn resource(&self) -> &str {
        &self.resource
    }

    pub fn register_client(
        &self,
        name: &str,
        redirect_uri: &str,
    ) -> Result<ClientCredentials, OAuthError> {
        if name.trim().is_empty()
            || name.len() > 128
            || name.chars().any(char::is_control)
            || !valid_redirect(redirect_uri)
        {
            return Err(OAuthError::InvalidRequest);
        }

        let connection = storage::open(&self.data_dir)?;
        let client_id = random_token("mcp_client_")?;
        let client_secret = random_token("mcp_secret_")?;

        connection.execute(
            "INSERT INTO mcp_oauth_clients
             (client_id, name, secret_hash, redirect_uri,
              allowed_scope, created_at_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, unixepoch())",
            params![
                client_id,
                name.trim(),
                &crypto::hash_handle(&client_secret)[..],
                redirect_uri,
                default_scope(),
            ],
        )?;

        Ok(ClientCredentials {
            client_id,
            client_secret,
        })
    }

    pub fn list_clients(&self) -> Result<Vec<ClientRecord>, OAuthError> {
        let connection = storage::open(&self.data_dir)?;

        let mut statement = connection.prepare(
            "SELECT client_id, name, redirect_uri, allowed_scope, is_enabled
             FROM mcp_oauth_clients
             ORDER BY created_at_unix, client_id",
        )?;

        let rows = statement.query_map([], |row| {
            Ok(ClientRecord {
                client_id: row.get(0)?,
                name: row.get(1)?,
                redirect_uri: row.get(2)?,
                allowed_scope: row.get(3)?,
                is_enabled: row.get(4)?,
            })
        })?;

        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn disable_client(&self, client_id: &str) -> Result<bool, OAuthError> {
        let mut connection = storage::open(&self.data_dir)?;

        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let changed = tx.execute(
            "UPDATE mcp_oauth_clients
             SET is_enabled = 0 WHERE client_id = ?1",
            params![client_id],
        )?;

        tx.execute(
            "UPDATE mcp_oauth_grants
             SET revoked_at_unix = unixepoch()
             WHERE client_id = ?1 AND revoked_at_unix IS NULL",
            params![client_id],
        )?;

        tx.commit()?;

        Ok(changed == 1)
    }

    /// Call only after authenticated browser authorization and consent.
    /// The HTTP layer must never accept an arbitrary user_id from the client.
    pub fn issue_code(
        &self,
        client_id: &str,
        redirect_uri: &str,
        challenge: &str,
        user_id: &str,
    ) -> Result<String, OAuthError> {
        if !valid_challenge(challenge) || !valid_redirect(redirect_uri) {
            return Err(OAuthError::InvalidRequest);
        }

        let mut connection = storage::open(&self.data_dir)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let code = self.issue_code_on(
            &tx,
            client_id,
            redirect_uri,
            challenge,
            user_id,
            &default_scope(),
        )?;

        tx.commit()?;

        Ok(code)
    }

    pub fn exchange_code(
        &self,
        client_id: &str,
        secret: &str,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<TokenPair, OAuthError> {
        let mut connection = storage::open(&self.data_dir)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        authenticate_client(&tx, client_id, secret)?;

        if !valid_token(code, "mcp_code_") {
            return Err(OAuthError::InvalidGrant);
        }

        let challenge = pkce_challenge(verifier).ok_or(OAuthError::InvalidGrant)?;

        let current_time = now(&tx)?;
        let hash = crypto::hash_handle(code);

        let record = tx
            .query_row(
                "SELECT g.id, g.expires_at_unix,
                    c.expires_at_unix, c.consumed_at_unix, g.scope
             FROM mcp_oauth_codes c
             JOIN mcp_oauth_grants g ON g.id = c.grant_id
             JOIN users u ON u.id = g.user_id
             WHERE c.code_hash = ?1
               AND g.client_id = ?2
               AND c.redirect_uri = ?3
               AND c.code_challenge = ?4
               AND g.resource = ?5
               AND g.revoked_at_unix IS NULL
               AND g.expires_at_unix > ?6
               AND u.issuer = ?7 AND u.is_enabled = 1",
                params![
                    &hash[..],
                    client_id,
                    redirect_uri,
                    challenge,
                    self.resource,
                    current_time,
                    self.user_issuer,
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?;

        let Some((grant_id, grant_expiry, code_expiry, consumed, scope)) = record else {
            return Err(OAuthError::InvalidGrant);
        };

        if parse_scopes(&scope).is_empty() {
            return Err(OAuthError::InvalidGrant);
        }

        if consumed.is_some() {
            revoke_grant(&tx, &grant_id)?;
            tx.commit()?;
            return Err(OAuthError::InvalidGrant);
        }

        if code_expiry <= current_time {
            return Err(OAuthError::InvalidGrant);
        }

        tx.execute(
            "UPDATE mcp_oauth_codes
             SET consumed_at_unix = ?1 WHERE code_hash = ?2",
            params![current_time, &hash[..]],
        )?;

        let pair = issue_pair(&tx, &grant_id, 0, current_time, grant_expiry, &scope)?;

        tx.commit()?;

        Ok(pair)
    }

    pub fn rotate_refresh(
        &self,
        client_id: &str,
        secret: &str,
        refresh_token: &str,
    ) -> Result<TokenPair, OAuthError> {
        let mut connection = storage::open(&self.data_dir)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        authenticate_client(&tx, client_id, secret)?;

        if !valid_token(refresh_token, "mcp_rt_") {
            return Err(OAuthError::InvalidGrant);
        }

        let current_time = now(&tx)?;
        let hash = crypto::hash_handle(refresh_token);

        let record = tx
            .query_row(
                "SELECT g.id, g.expires_at_unix,
                    r.generation, r.consumed_at_unix, g.scope
             FROM mcp_refresh_tokens r
             JOIN mcp_oauth_grants g ON g.id = r.grant_id
             JOIN users u ON u.id = g.user_id
             WHERE r.token_hash = ?1
               AND g.client_id = ?2
               AND g.resource = ?3
               AND g.revoked_at_unix IS NULL
               AND r.revoked_at_unix IS NULL
               AND g.expires_at_unix > ?4
               AND r.expires_at_unix > ?4
               AND u.issuer = ?5 AND u.is_enabled = 1",
                params![
                    &hash[..],
                    client_id,
                    self.resource,
                    current_time,
                    self.user_issuer,
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?;

        let Some((grant_id, expiry, generation, consumed, scope)) = record else {
            return Err(OAuthError::InvalidGrant);
        };

        if consumed.is_some() {
            // A replay invalidates the entire token family.
            revoke_grant(&tx, &grant_id)?;
            tx.commit()?;
            return Err(OAuthError::InvalidGrant);
        }

        let next_generation = generation.checked_add(1).ok_or(OAuthError::InvalidGrant)?;

        tx.execute(
            "UPDATE mcp_refresh_tokens
             SET consumed_at_unix = ?1 WHERE token_hash = ?2",
            params![current_time, &hash[..]],
        )?;

        let pair = issue_pair(
            &tx,
            &grant_id,
            next_generation,
            current_time,
            expiry,
            &scope,
        )?;

        tx.commit()?;

        Ok(pair)
    }

    pub fn verify_access(&self, token: &str) -> Result<Option<AccessIdentity>, OAuthError> {
        if !valid_token(token, "mcp_at_") {
            return Ok(None);
        }

        let connection = storage::open(&self.data_dir)?;

        let record = connection
            .query_row(
                "SELECT u.id, u.subject, u.role, g.id, g.scope, c.allowed_scope
                 FROM mcp_access_tokens a
                 JOIN mcp_oauth_grants g ON g.id = a.grant_id
                 JOIN users u ON u.id = g.user_id
                 JOIN mcp_oauth_clients c ON c.client_id = g.client_id
                 WHERE a.token_hash = ?1
                   AND a.expires_at_unix > unixepoch()
                   AND g.expires_at_unix > unixepoch()
                   AND a.revoked_at_unix IS NULL
                   AND g.revoked_at_unix IS NULL
                   AND u.is_enabled = 1 AND c.is_enabled = 1
                   AND g.resource = ?2
                   AND u.issuer = ?3",
                params![
                    &crypto::hash_handle(token)[..],
                    self.resource,
                    self.user_issuer,
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;

        let Some((user_id, subject, role, grant_id, scope, allowed_scope)) = record else {
            return Ok(None);
        };

        // A grant is only honoured while it stays inside the client's allowed
        // scope set. This is what stops a client from widening its own rights.
        let scopes = parse_scopes(&scope);
        let allowed = parse_scopes(&allowed_scope);

        if !scope_subset(&scopes, &allowed) {
            return Ok(None);
        }

        Ok(Some(AccessIdentity {
            user_id,
            subject,
            role,
            grant_id,
            scopes,
        }))
    }
    pub fn revoke(&self, client_id: &str, secret: &str, token: &str) -> Result<(), OAuthError> {
        let mut connection = storage::open(&self.data_dir)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        authenticate_client(&tx, client_id, secret)?;

        if valid_token(token, "mcp_at_") || valid_token(token, "mcp_rt_") {
            tx.execute(
                "UPDATE mcp_oauth_grants
                 SET revoked_at_unix = unixepoch()
                 WHERE client_id = ?1 AND resource = ?2
                   AND revoked_at_unix IS NULL
                   AND id IN (
                       SELECT grant_id FROM mcp_access_tokens
                       WHERE token_hash = ?3
                       UNION
                       SELECT grant_id FROM mcp_refresh_tokens
                       WHERE token_hash = ?3
                   )",
                params![client_id, self.resource, &crypto::hash_handle(token)[..],],
            )?;
        }

        // Unknown tokens also return success (RFC 7009).
        tx.commit()?;

        Ok(())
    }
}

impl OAuthStore {
    #[allow(clippy::too_many_arguments)]
    fn issue_code_on(
        &self,
        connection: &Connection,
        client_id: &str,
        redirect_uri: &str,
        challenge: &str,
        user_id: &str,
        scopes: &str,
    ) -> Result<String, OAuthError> {
        let current_time = now(connection)?;
        let grant_id = Uuid::new_v4().to_string();
        let code = random_token("mcp_code_")?;

        let changed = connection.execute(
            "INSERT INTO mcp_oauth_grants
             (id, user_id, client_id, resource, scope,
              created_at_unix, expires_at_unix)
             SELECT ?1, u.id, c.client_id, ?2, ?3, ?4, ?5
             FROM users u CROSS JOIN mcp_oauth_clients c
             WHERE u.id = ?6 AND u.issuer = ?7
               AND u.is_enabled = 1
               AND c.client_id = ?8 AND c.is_enabled = 1
               AND c.redirect_uri = ?9",
            params![
                grant_id,
                self.resource,
                scopes,
                current_time,
                current_time + GRANT_TTL,
                user_id,
                self.user_issuer,
                client_id,
                redirect_uri,
            ],
        )?;

        if changed != 1 {
            return Err(OAuthError::AccessDenied);
        }

        connection.execute(
            "INSERT INTO mcp_oauth_codes
             (code_hash, grant_id, redirect_uri, code_challenge,
              created_at_unix, expires_at_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                &crypto::hash_handle(&code)[..],
                grant_id,
                redirect_uri,
                challenge,
                current_time,
                current_time + CODE_TTL,
            ],
        )?;

        Ok(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUBJECT: &str = "550e8400-e29b-41d4-a716-446655440000";

    const REDIRECT: &str = "https://chatgpt.com/connector_platform_oauth_redirect";

    const VERIFIER: &str = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG";

    fn setup() -> (tempfile::TempDir, OAuthStore, SsoConfig) {
        let dir = tempfile::tempdir().unwrap();

        storage::initialize(dir.path()).unwrap();

        let config = SsoConfig {
            issuer: "https://auth.alxprgs.tech".to_owned(),
            client_id: "test-client".to_owned(),
            redirect_uri: "https://workspace.example.com/auth/callback".to_owned(),
            trusted_proxies: Vec::new(),
        };

        let connection = storage::open(dir.path()).unwrap();

        connection
            .execute(
                "INSERT INTO users
             (id, issuer, subject, role, is_enabled,
              created_at_unix, updated_at_unix)
             VALUES ('user-1', ?1, ?2, 'admin', 1,
                     unixepoch(), unixepoch())",
                params![config.issuer, SUBJECT],
            )
            .unwrap();

        let store = OAuthStore::new(dir.path(), &config).unwrap();

        (dir, store, config)
    }

    fn client(store: &OAuthStore) -> ClientCredentials {
        store.register_client("ChatGPT", REDIRECT).unwrap()
    }

    fn code(store: &OAuthStore, client: &ClientCredentials) -> String {
        store
            .issue_code(
                &client.client_id,
                REDIRECT,
                &pkce_challenge(VERIFIER).unwrap(),
                "user-1",
            )
            .unwrap()
    }

    fn tokens(store: &OAuthStore, client: &ClientCredentials) -> TokenPair {
        store
            .exchange_code(
                &client.client_id,
                &client.client_secret,
                &code(store, client),
                REDIRECT,
                VERIFIER,
            )
            .unwrap()
    }

    #[test]
    fn registers_client_without_storing_plaintext_secret() {
        let (dir, store, _) = setup();
        let client = client(&store);

        let connection = storage::open(dir.path()).unwrap();

        let hash: Vec<u8> = connection
            .query_row(
                "SELECT secret_hash FROM mcp_oauth_clients
             WHERE client_id = ?1",
                params![client.client_id],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(hash, crypto::hash_handle(&client.client_secret));
        assert_eq!(store.list_clients().unwrap().len(), 1);

        assert!(
            store
                .register_client("Invalid", "http://example.com/callback")
                .is_err()
        );
    }

    #[test]
    fn exchanges_code_only_for_matching_client_redirect_and_pkce() {
        let (_dir, store, _) = setup();
        let client = client(&store);
        let code = code(&store, &client);

        assert!(
            store
                .exchange_code(&client.client_id, "wrong-secret", &code, REDIRECT, VERIFIER,)
                .is_err()
        );

        assert!(
            store
                .exchange_code(
                    &client.client_id,
                    &client.client_secret,
                    &code,
                    "https://example.com/callback",
                    VERIFIER,
                )
                .is_err()
        );

        assert!(
            store
                .exchange_code(
                    &client.client_id,
                    &client.client_secret,
                    &code,
                    REDIRECT,
                    &"x".repeat(43),
                )
                .is_err()
        );

        let tokens = store
            .exchange_code(
                &client.client_id,
                &client.client_secret,
                &code,
                REDIRECT,
                VERIFIER,
            )
            .unwrap();

        assert_eq!(tokens.expires_in, ACCESS_TTL);

        let identity = store.verify_access(&tokens.access_token).unwrap().unwrap();

        assert_eq!(identity.subject, SUBJECT);
        assert_eq!(identity.role, "admin");

        assert!(
            store
                .verify_access(&tokens.refresh_token)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn authorization_code_replay_revokes_grant() {
        let (_dir, store, _) = setup();
        let client = client(&store);
        let code = code(&store, &client);

        let tokens = store
            .exchange_code(
                &client.client_id,
                &client.client_secret,
                &code,
                REDIRECT,
                VERIFIER,
            )
            .unwrap();

        assert!(
            store
                .exchange_code(
                    &client.client_id,
                    &client.client_secret,
                    &code,
                    REDIRECT,
                    VERIFIER,
                )
                .is_err()
        );

        assert!(store.verify_access(&tokens.access_token).unwrap().is_none());
    }

    #[test]
    fn refresh_rotation_detects_replay() {
        let (_dir, store, _) = setup();
        let client = client(&store);
        let first = tokens(&store, &client);

        let second = store
            .rotate_refresh(
                &client.client_id,
                &client.client_secret,
                &first.refresh_token,
            )
            .unwrap();

        assert_ne!(first.refresh_token, second.refresh_token);
        assert_ne!(first.access_token, second.access_token);

        assert!(store.verify_access(&second.access_token).unwrap().is_some());

        assert!(
            store
                .rotate_refresh(
                    &client.client_id,
                    &client.client_secret,
                    &first.refresh_token,
                )
                .is_err()
        );

        assert!(store.verify_access(&second.access_token).unwrap().is_none());

        assert!(
            store
                .rotate_refresh(
                    &client.client_id,
                    &client.client_secret,
                    &second.refresh_token,
                )
                .is_err()
        );
    }

    #[test]
    fn revocation_disables_the_token_family() {
        let (_dir, store, _) = setup();
        let client = client(&store);
        let tokens = tokens(&store, &client);

        store
            .revoke(
                &client.client_id,
                &client.client_secret,
                &tokens.refresh_token,
            )
            .unwrap();

        assert!(store.verify_access(&tokens.access_token).unwrap().is_none());

        assert!(
            store
                .rotate_refresh(
                    &client.client_id,
                    &client.client_secret,
                    &tokens.refresh_token,
                )
                .is_err()
        );
    }

    #[test]
    fn disabling_client_revokes_its_tokens() {
        let (_dir, store, _) = setup();
        let client = client(&store);
        let tokens = tokens(&store, &client);

        assert!(store.disable_client(&client.client_id).unwrap());

        assert!(store.verify_access(&tokens.access_token).unwrap().is_none());

        assert!(
            store
                .rotate_refresh(
                    &client.client_id,
                    &client.client_secret,
                    &tokens.refresh_token,
                )
                .is_err()
        );
    }

    #[test]
    fn disabled_user_cannot_restore_old_grants() {
        let (dir, store, config) = setup();
        let client = client(&store);
        let tokens = tokens(&store, &client);

        crate::users::disable_access(dir.path(), &config, SUBJECT).unwrap();

        assert!(store.verify_access(&tokens.access_token).unwrap().is_none());

        crate::users::grant_access(dir.path(), &config, SUBJECT, "admin").unwrap();

        assert!(store.verify_access(&tokens.access_token).unwrap().is_none());

        assert!(
            store
                .rotate_refresh(
                    &client.client_id,
                    &client.client_secret,
                    &tokens.refresh_token,
                )
                .is_err()
        );
    }

    #[test]
    fn expired_access_and_codes_are_rejected() {
        let (dir, store, _) = setup();
        let client = client(&store);
        let tokens = tokens(&store, &client);
        let code = code(&store, &client);

        let connection = storage::open(dir.path()).unwrap();

        connection
            .execute_batch(
                "UPDATE mcp_access_tokens
             SET created_at_unix = 1, expires_at_unix = 2;

             UPDATE mcp_oauth_codes
             SET created_at_unix = 1, expires_at_unix = 2;",
            )
            .unwrap();

        assert!(store.verify_access(&tokens.access_token).unwrap().is_none());

        assert!(
            store
                .exchange_code(
                    &client.client_id,
                    &client.client_secret,
                    &code,
                    REDIRECT,
                    VERIFIER,
                )
                .is_err()
        );
    }

    fn race_two<F>(operation: F) -> [Result<TokenPair, OAuthError>; 2]
    where
        F: Fn() -> Result<TokenPair, OAuthError> + Sync,
    {
        let barrier = std::sync::Barrier::new(2);

        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                barrier.wait();
                operation()
            });

            let second = scope.spawn(|| {
                barrier.wait();
                operation()
            });

            [first.join().unwrap(), second.join().unwrap()]
        })
    }

    fn assert_replay_revoked(store: &OAuthStore, results: [Result<TokenPair, OAuthError>; 2]) {
        let mut winner = None;

        for result in results {
            match result {
                Ok(pair) => {
                    assert!(winner.replace(pair).is_none());
                }
                Err(OAuthError::InvalidGrant) => {}
                Err(error) => panic!("Unexpected OAuth error: {error}"),
            }
        }

        let pair = winner.expect("One exchange must succeed");

        assert!(store.verify_access(&pair.access_token).unwrap().is_none());
    }

    #[test]
    fn concurrent_code_exchange_detects_replay() {
        let (_dir, store, _) = setup();
        let client = client(&store);
        let authorization_code = code(&store, &client);

        let results = race_two(|| {
            store.exchange_code(
                &client.client_id,
                &client.client_secret,
                &authorization_code,
                REDIRECT,
                VERIFIER,
            )
        });

        assert_replay_revoked(&store, results);
    }

    #[test]
    fn concurrent_refresh_rotation_detects_replay() {
        let (_dir, store, _) = setup();
        let client = client(&store);
        let first = tokens(&store, &client);

        let results = race_two(|| {
            store.rotate_refresh(
                &client.client_id,
                &client.client_secret,
                &first.refresh_token,
            )
        });

        assert_replay_revoked(&store, results);

        assert!(store.verify_access(&first.access_token).unwrap().is_none());
    }

    #[test]
    fn invalid_client_is_rejected_before_invalid_grant() {
        let (_dir, store, _) = setup();

        assert!(matches!(
            store.exchange_code("invalid", "invalid", "invalid", REDIRECT, "invalid",),
            Err(OAuthError::InvalidClient)
        ));

        assert!(matches!(
            store.rotate_refresh("invalid", "invalid", "invalid",),
            Err(OAuthError::InvalidClient)
        ));
    }
}
