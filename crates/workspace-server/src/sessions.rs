use super::*;
use crate::{
    sso::{LoginResult, SessionTokens},
    users::{self, UserRecord},
};

const SESSION_TTL_SECONDS: i64 = 28_800;

pub struct SessionCredentials {
    pub handle: String,
    pub csrf_token: String,
    pub expires_at_unix: i64,
}

#[derive(Debug, Serialize)]
pub struct SessionInfo {
    pub user: UserRecord,
    pub expires_at_unix: i64,
    pub access_expires_at_unix: i64,
}

fn valid_handle(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn session_aad(
    config: &SsoConfig,
    handle_hash: &[u8; 32],
    user_id: &str,
    expires_at_unix: i64,
) -> Vec<u8> {
    let mut aad = b"workspace:web-session:v1\0".to_vec();

    aad.extend_from_slice(handle_hash);
    aad.extend_from_slice(user_id.as_bytes());
    aad.push(0);
    aad.extend_from_slice(config.issuer.as_bytes());
    aad.push(0);
    aad.extend_from_slice(config.client_id.as_bytes());
    aad.push(0);
    aad.extend_from_slice(config.redirect_uri.as_bytes());
    aad.push(0);
    aad.extend_from_slice(&expires_at_unix.to_be_bytes());

    aad
}

impl AuthStore {
    pub fn create_session(
        &self,
        config: &SsoConfig,
        login: &LoginResult,
    ) -> Result<Option<SessionCredentials>, AuthStoreError> {
        let mut connection = storage::open(&self.data_dir)?;

        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let now: i64 = tx.query_row("SELECT unixepoch()", [], |row| row.get(0))?;

        let expires_at = now + SESSION_TTL_SECONDS;

        let access_expires_at = login.tokens.access_expires_at_unix;

        if access_expires_at <= now
            || access_expires_at > expires_at
            || access_expires_at > login.identity.expires_at_unix
        {
            return Err(AuthStoreError::InvalidPayload);
        }

        let user = users::record_login(&tx, config, login)?;

        if !user.is_enabled {
            tx.commit()?;
            return Ok(None);
        }

        let handle = random_handle()?;
        let csrf_token = random_handle()?;

        let handle_hash = hash_handle(&handle);
        let csrf_hash = hash_handle(&csrf_token);

        let plaintext =
            serde_json::to_vec(&login.tokens).map_err(|_| AuthStoreError::InvalidPayload)?;

        let encrypted = self.cipher.seal(
            &plaintext,
            &session_aad(config, &handle_hash, &user.id, expires_at),
        )?;

        tx.execute(
            "DELETE FROM web_sessions
             WHERE expires_at_unix <= unixepoch()
                OR revoked_at_unix IS NOT NULL",
            [],
        )?;

        tx.execute(
            "INSERT INTO web_sessions (
                handle_hash, user_id, client_id, csrf_hash,
                encrypted_tokens, created_at_unix,
                expires_at_unix, access_expires_at_unix
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8
             )",
            params![
                &handle_hash[..],
                &user.id,
                &config.client_id,
                &csrf_hash[..],
                encrypted,
                now,
                expires_at,
                access_expires_at,
            ],
        )?;

        tx.commit()?;

        Ok(Some(SessionCredentials {
            handle,
            csrf_token,
            expires_at_unix: expires_at,
        }))
    }

    pub fn get_session(
        &self,
        config: &SsoConfig,
        handle: &str,
        csrf_token: Option<&str>,
    ) -> Result<Option<SessionInfo>, AuthStoreError> {
        sso::validate_config(config).map_err(|_| AuthStoreError::InvalidFlow)?;

        if !valid_handle(handle) || csrf_token.is_some_and(|token| !valid_handle(token)) {
            return Ok(None);
        }

        let handle_hash = hash_handle(handle);
        let csrf_hash = csrf_token.map(hash_handle);

        let connection = storage::open(&self.data_dir)?;

        let record = connection
            .query_row(
                "SELECT
                u.id, u.issuer, u.subject,
                u.username, u.role, u.is_enabled,
                s.expires_at_unix,
                s.access_expires_at_unix,
                s.encrypted_tokens
             FROM web_sessions s
             JOIN users u ON u.id = s.user_id
             WHERE s.handle_hash = ?1
               AND s.client_id = ?2
               AND u.issuer = ?3
               AND u.is_enabled = 1
               AND s.revoked_at_unix IS NULL
               AND s.refresh_started_at_unix IS NULL
               AND s.generation = 0
               AND s.expires_at_unix > unixepoch()
               AND s.access_expires_at_unix > unixepoch()
               AND (?4 IS NULL OR s.csrf_hash = ?4)",
                params![
                    &handle_hash[..],
                    &config.client_id,
                    &config.issuer,
                    csrf_hash.as_ref().map(|hash| &hash[..]),
                ],
                |row| {
                    Ok((
                        SessionInfo {
                            user: users::read_user(row)?,
                            expires_at_unix: row.get(6)?,
                            access_expires_at_unix: row.get(7)?,
                        },
                        row.get::<_, Vec<u8>>(8)?,
                    ))
                },
            )
            .optional()?;

        let Some((info, encrypted)) = record else {
            return Ok(None);
        };

        let plaintext = self.cipher.open(
            &encrypted,
            &session_aad(config, &handle_hash, &info.user.id, info.expires_at_unix),
        )?;

        let tokens: SessionTokens =
            serde_json::from_slice(&plaintext).map_err(|_| AuthStoreError::InvalidPayload)?;

        if tokens.access_expires_at_unix != info.access_expires_at_unix {
            return Err(AuthStoreError::InvalidPayload);
        }

        Ok(Some(info))
    }

    pub fn revoke_session(
        &self,
        config: &SsoConfig,
        handle: &str,
        csrf_token: &str,
    ) -> Result<bool, AuthStoreError> {
        sso::validate_config(config).map_err(|_| AuthStoreError::InvalidFlow)?;

        if !valid_handle(handle) || !valid_handle(csrf_token) {
            return Ok(false);
        }

        let handle_hash = hash_handle(handle);
        let csrf_hash = hash_handle(csrf_token);

        let connection = storage::open(&self.data_dir)?;

        let changed = connection.execute(
            "DELETE FROM web_sessions
             WHERE handle_hash = ?1
               AND client_id = ?2
               AND csrf_hash = ?3
               AND user_id IN (
                   SELECT id FROM users WHERE issuer = ?4
               )",
            params![
                &handle_hash[..],
                &config.client_id,
                &csrf_hash[..],
                &config.issuer,
            ],
        )?;

        Ok(changed == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sso::VerifiedIdentity;
    use std::time::{SystemTime, UNIX_EPOCH};

    const KEY: [u8; 32] = [42; 32];
    const SUBJECT: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn config() -> SsoConfig {
        SsoConfig {
            issuer: "https://auth.alxprgs.tech".to_owned(),
            client_id: "test-client".to_owned(),
            redirect_uri: "https://workspace.example.com/auth/callback".to_owned(),
        }
    }

    fn login() -> LoginResult {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        LoginResult {
            identity: VerifiedIdentity {
                issuer: "https://auth.alxprgs.tech".to_owned(),
                subject: SUBJECT.to_owned(),
                expires_at_unix: now + 300,
            },
            username: Some("test-user".to_owned()),
            email: Some("test@example.com".to_owned()),
            email_verified: true,
            tokens: SessionTokens {
                access_token: "test-access-token".to_owned(),
                refresh_token: "test-refresh-token".to_owned(),
                id_token: "test-id-token".to_owned(),
                access_expires_at_unix: now + 300,
            },
        }
    }

    fn setup() -> (tempfile::TempDir, AuthStore, SsoConfig) {
        let dir = tempfile::tempdir().unwrap();

        storage::initialize(dir.path()).unwrap();

        let store = AuthStore::new(dir.path(), &KEY).unwrap();

        (dir, store, config())
    }

    #[test]
    fn unknown_user_has_no_automatic_access() {
        let (dir, store, config) = setup();

        assert!(store.create_session(&config, &login()).unwrap().is_none());

        let users = users::list_users(dir.path(), &config).unwrap();

        assert_eq!(users.len(), 1);
        assert_eq!(users[0].role, "user");
        assert!(!users[0].is_enabled);

        let connection = storage::open(dir.path()).unwrap();

        let count: i64 = connection
            .query_row("SELECT count(*) FROM web_sessions", [], |row| row.get(0))
            .unwrap();

        assert_eq!(count, 0);
    }

    #[test]
    fn approved_user_can_create_session() {
        let (dir, store, config) = setup();

        users::grant_access(dir.path(), &config, SUBJECT, "admin").unwrap();

        let credentials = store.create_session(&config, &login()).unwrap().unwrap();

        let info = store
            .get_session(&config, &credentials.handle, None)
            .unwrap()
            .unwrap();

        assert_eq!(info.user.subject, SUBJECT);
        assert_eq!(info.user.role, "admin");
        assert!(info.user.is_enabled);

        let connection = storage::open(dir.path()).unwrap();

        let encrypted: Vec<u8> = connection
            .query_row("SELECT encrypted_tokens FROM web_sessions", [], |row| {
                row.get(0)
            })
            .unwrap();

        let secret = b"test-refresh-token";

        assert!(!encrypted.windows(secret.len()).any(|part| part == secret));
    }

    #[test]
    fn session_checks_csrf_and_client_binding() {
        let (dir, store, config) = setup();

        users::grant_access(dir.path(), &config, SUBJECT, "user").unwrap();

        let credentials = store.create_session(&config, &login()).unwrap().unwrap();

        let wrong = random_handle().unwrap();

        assert!(
            store
                .get_session(&config, &credentials.handle, Some(&wrong),)
                .unwrap()
                .is_none()
        );

        assert!(
            store
                .get_session(&config, &credentials.handle, Some(&credentials.csrf_token),)
                .unwrap()
                .is_some()
        );

        let mut other = config.clone();
        other.client_id = "other-client".to_owned();

        assert!(
            store
                .get_session(&other, &credentials.handle, None,)
                .unwrap()
                .is_none()
        );

        assert!(store.get_session(&config, &wrong, None,).unwrap().is_none());
    }

    #[test]
    fn logout_requires_correct_csrf() {
        let (dir, store, config) = setup();

        users::grant_access(dir.path(), &config, SUBJECT, "user").unwrap();

        let credentials = store.create_session(&config, &login()).unwrap().unwrap();

        let wrong = random_handle().unwrap();

        assert!(
            !store
                .revoke_session(&config, &credentials.handle, &wrong,)
                .unwrap()
        );

        assert!(
            store
                .revoke_session(&config, &credentials.handle, &credentials.csrf_token,)
                .unwrap()
        );

        assert!(
            store
                .get_session(&config, &credentials.handle, None,)
                .unwrap()
                .is_none()
        );

        assert!(
            !store
                .revoke_session(&config, &credentials.handle, &credentials.csrf_token,)
                .unwrap()
        );
    }

    #[test]
    fn disabling_user_revokes_existing_sessions() {
        let (dir, store, config) = setup();

        users::grant_access(dir.path(), &config, SUBJECT, "admin").unwrap();

        let credentials = store.create_session(&config, &login()).unwrap().unwrap();

        assert!(users::disable_access(dir.path(), &config, SUBJECT,).unwrap());

        assert!(
            store
                .get_session(&config, &credentials.handle, None,)
                .unwrap()
                .is_none()
        );

        assert!(store.create_session(&config, &login()).unwrap().is_none());

        users::grant_access(dir.path(), &config, SUBJECT, "user").unwrap();

        assert!(
            store
                .get_session(&config, &credentials.handle, None,)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn changing_role_invalidates_existing_sessions() {
        let (dir, store, config) = setup();

        users::grant_access(dir.path(), &config, SUBJECT, "admin").unwrap();

        let credentials = store.create_session(&config, &login()).unwrap().unwrap();

        users::grant_access(dir.path(), &config, SUBJECT, "user").unwrap();

        assert!(
            store
                .get_session(&config, &credentials.handle, None,)
                .unwrap()
                .is_none()
        );

        let credentials = store.create_session(&config, &login()).unwrap().unwrap();

        let info = store
            .get_session(&config, &credentials.handle, None)
            .unwrap()
            .unwrap();

        assert_eq!(info.user.role, "user");
    }

    #[test]
    fn session_rejects_wrong_key_and_expired_access() {
        let (dir, store, config) = setup();

        users::grant_access(dir.path(), &config, SUBJECT, "user").unwrap();

        let credentials = store.create_session(&config, &login()).unwrap().unwrap();

        let wrong_key = AuthStore::new(dir.path(), &[43; 32]).unwrap();

        assert!(
            wrong_key
                .get_session(&config, &credentials.handle, None,)
                .is_err()
        );

        let connection = storage::open(dir.path()).unwrap();

        connection
            .execute(
                "UPDATE web_sessions
             SET access_expires_at_unix = unixepoch() - 1",
                [],
            )
            .unwrap();

        assert!(
            store
                .get_session(&config, &credentials.handle, None,)
                .unwrap()
                .is_none()
        );
    }
}
