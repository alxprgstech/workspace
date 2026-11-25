use super::*;
use crate::{
    sso::{LoginResult, RefreshedSession, SessionTokens},
    users::{self, UserRecord},
};

#[path = "refresh_store.rs"]
mod refresh_store;

use refresh_store::generation_aad;
pub use refresh_store::{RefreshDecision, RefreshLease};

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
                s.encrypted_tokens,
                s.generation
             FROM web_sessions s
             JOIN users u ON u.id = s.user_id
             WHERE s.handle_hash = ?1
               AND s.client_id = ?2
               AND u.issuer = ?3
               AND u.is_enabled = 1
               AND s.revoked_at_unix IS NULL
               AND s.refresh_started_at_unix IS NULL
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
                        row.get::<_, i64>(9)?,
                    ))
                },
            )
            .optional()?;

        let Some((info, encrypted, generation)) = record else {
            return Ok(None);
        };

        let plaintext = match self.cipher.open(
            &encrypted,
            &generation_aad(
                config,
                &handle_hash,
                &info.user.id,
                info.expires_at_unix,
                generation,
            ),
        ) {
            Ok(plaintext) => plaintext,
            Err(_) => {
                // A damaged session can never become valid again.
                let _ = connection.execute(
                    "DELETE FROM web_sessions
                     WHERE handle_hash = ?1",
                    params![&handle_hash[..]],
                );

                return Ok(None);
            }
        };

        let Ok(tokens) = serde_json::from_slice::<SessionTokens>(&plaintext) else {
            let _ = connection.execute(
                "DELETE FROM web_sessions
                 WHERE handle_hash = ?1",
                params![&handle_hash[..]],
            );

            return Ok(None);
        };

        if tokens.access_expires_at_unix != info.access_expires_at_unix {
            let _ = connection.execute(
                "DELETE FROM web_sessions
                 WHERE handle_hash = ?1",
                params![&handle_hash[..]],
            );

            return Ok(None);
        }

        Ok(Some(info))
    }

    pub fn revoke_session(
        &self,
        config: &SsoConfig,
        handle: &str,
        csrf_token: &str,
    ) -> Result<Option<SessionTokens>, AuthStoreError> {
        sso::validate_config(config).map_err(|_| AuthStoreError::InvalidFlow)?;

        if !valid_handle(handle) || !valid_handle(csrf_token) {
            return Ok(None);
        }

        let handle_hash = hash_handle(handle);
        let csrf_hash = hash_handle(csrf_token);

        let mut connection = storage::open(&self.data_dir)?;

        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let record = tx
            .query_row(
                "SELECT user_id, expires_at_unix, encrypted_tokens, generation
                 FROM web_sessions
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
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;

        let Some((user_id, expires_at, encrypted, generation)) = record else {
            return Ok(None);
        };

        let tokens = self
            .cipher
            .open(
                &encrypted,
                &generation_aad(config, &handle_hash, &user_id, expires_at, generation),
            )
            .ok()
            .and_then(|plaintext| serde_json::from_slice::<SessionTokens>(&plaintext).ok())
            .filter(|tokens| !tokens.refresh_token.is_empty());

        tx.execute(
            "DELETE FROM web_sessions
             WHERE handle_hash = ?1",
            params![&handle_hash[..]],
        )?;

        tx.commit()?;

        Ok(tokens)
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
            store
                .revoke_session(&config, &credentials.handle, &wrong,)
                .unwrap()
                .is_none()
        );

        assert!(
            store
                .revoke_session(&config, &credentials.handle, &credentials.csrf_token,)
                .unwrap()
                .is_some()
        );

        assert!(
            store
                .get_session(&config, &credentials.handle, None,)
                .unwrap()
                .is_none()
        );

        assert!(
            store
                .revoke_session(&config, &credentials.handle, &credentials.csrf_token,)
                .unwrap()
                .is_none()
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

        // A session that cannot be decrypted is discarded, not retried.
        assert!(
            wrong_key
                .get_session(&config, &credentials.handle, None,)
                .unwrap()
                .is_none()
        );

        let connection = storage::open(dir.path()).unwrap();

        let remaining: i64 = connection
            .query_row("SELECT count(*) FROM web_sessions", [], |row| row.get(0))
            .unwrap();

        assert_eq!(remaining, 0);

        let credentials = store.create_session(&config, &login()).unwrap().unwrap();

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

    fn near_expiry_session(
        dir: &std::path::Path,
        store: &AuthStore,
        config: &SsoConfig,
    ) -> SessionCredentials {
        users::grant_access(dir, config, SUBJECT, "admin").unwrap();

        let mut login = login();

        login.tokens.access_expires_at_unix -= 275;

        store.create_session(config, &login).unwrap().unwrap()
    }

    fn acquire_refresh(
        store: &AuthStore,
        config: &SsoConfig,
        credentials: &SessionCredentials,
    ) -> Box<RefreshLease> {
        match store
            .begin_refresh(config, &credentials.handle, None)
            .unwrap()
        {
            RefreshDecision::Acquired(lease) => lease,
            _ => panic!("Expected an acquired refresh lease"),
        }
    }

    fn rotated_tokens() -> RefreshedSession {
        let mut tokens = login().tokens;

        tokens.access_token = "rotated-access-token".to_owned();
        tokens.refresh_token = "rotated-refresh-token".to_owned();
        tokens.id_token = "rotated-id-token".to_owned();

        RefreshedSession {
            tokens,
            profile: crate::sso::UserProfile {
                username: Some("rotated-user".to_owned()),
                email: Some("rotated@example.com".to_owned()),
                email_verified: Some(true),
            },
        }
    }

    #[test]
    fn fresh_session_does_not_require_refresh() {
        let (dir, store, config) = setup();

        users::grant_access(dir.path(), &config, SUBJECT, "admin").unwrap();

        let credentials = store.create_session(&config, &login()).unwrap().unwrap();

        assert!(matches!(
            store
                .begin_refresh(&config, &credentials.handle, None,)
                .unwrap(),
            RefreshDecision::NotNeeded
        ));
    }

    #[test]
    fn concurrent_refresh_is_acquired_only_once() {
        let (dir, store, config) = setup();

        let credentials = near_expiry_session(dir.path(), &store, &config);

        let results = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..2)
                .map(|_| {
                    let path = dir.path();
                    let config = &config;
                    let handle = &credentials.handle;

                    scope.spawn(move || {
                        let store = AuthStore::new(path, &KEY).unwrap();

                        match store.begin_refresh(config, handle, None).unwrap() {
                            RefreshDecision::Acquired(_) => 1,
                            RefreshDecision::Pending => 0,
                            _ => panic!("Unexpected refresh state"),
                        }
                    })
                })
                .collect();

            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .sum::<u32>()
        });

        assert_eq!(results, 1);
    }

    #[test]
    fn successful_refresh_increments_generation() {
        let (dir, store, config) = setup();

        let credentials = near_expiry_session(dir.path(), &store, &config);

        let lease = acquire_refresh(&store, &config, &credentials);

        assert_eq!(lease.subject(), SUBJECT);
        assert_eq!(lease.refresh_token(), "test-refresh-token");

        assert!(store.finish_refresh(&lease, rotated_tokens(),).unwrap());

        let info = store
            .get_session(&config, &credentials.handle, None)
            .unwrap()
            .unwrap();

        assert_eq!(info.expires_at_unix, credentials.expires_at_unix);

        let connection = storage::open(dir.path()).unwrap();

        let state: (i64, Option<i64>) = connection
            .query_row(
                "SELECT generation, refresh_started_at_unix
                 FROM web_sessions",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(state, (1, None));

        // A stale completion or cancellation cannot
        // overwrite or delete the new generation.
        assert!(!store.finish_refresh(&lease, rotated_tokens(),).unwrap());

        assert!(!store.abort_refresh(&lease).unwrap());

        assert!(
            store
                .get_session(&config, &credentials.handle, None,)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn ambiguous_refresh_revokes_session() {
        let (dir, store, config) = setup();

        let credentials = near_expiry_session(dir.path(), &store, &config);

        let lease = acquire_refresh(&store, &config, &credentials);

        assert!(store.abort_refresh(&lease).unwrap());

        assert!(matches!(
            store
                .begin_refresh(&config, &credentials.handle, None,)
                .unwrap(),
            RefreshDecision::Missing
        ));
    }

    #[test]
    fn unsubmitted_refresh_can_be_released_safely() {
        let (dir, store, config) = setup();

        let credentials = near_expiry_session(dir.path(), &store, &config);

        let lease = acquire_refresh(&store, &config, &credentials);

        // Releasing before any exchange leaves the session usable.
        assert!(store.release_unsubmitted_refresh(&lease).unwrap());

        let info = store
            .get_session(&config, &credentials.handle, None)
            .unwrap()
            .unwrap();

        assert_eq!(info.expires_at_unix, credentials.expires_at_unix);

        // The released lease can never complete a refresh.
        assert!(!store.finish_refresh(&lease, rotated_tokens(),).unwrap());

        // A release never clears another in-flight attempt.
        let connection = storage::open(dir.path()).unwrap();

        connection
            .execute(
                "UPDATE web_sessions
                 SET refresh_started_at_unix = unixepoch() + 1",
                [],
            )
            .unwrap();

        drop(connection);

        assert!(!store.release_unsubmitted_refresh(&lease).unwrap());

        // The session can retry once nothing is in flight anymore.
        let connection = storage::open(dir.path()).unwrap();

        connection
            .execute(
                "UPDATE web_sessions
                 SET refresh_started_at_unix = NULL",
                [],
            )
            .unwrap();

        drop(connection);

        let retry = acquire_refresh(&store, &config, &credentials);

        assert!(store.finish_refresh(&retry, rotated_tokens(),).unwrap());
    }

    #[test]
    fn abandoned_refresh_is_never_reacquired() {
        let (dir, store, config) = setup();

        let credentials = near_expiry_session(dir.path(), &store, &config);

        let lease = acquire_refresh(&store, &config, &credentials);

        let connection = storage::open(dir.path()).unwrap();

        connection
            .execute(
                "UPDATE web_sessions
             SET refresh_started_at_unix = unixepoch() - 121",
                [],
            )
            .unwrap();

        // Simulate restarting the Workspace process.
        let reopened = AuthStore::new(dir.path(), &KEY).unwrap();

        assert!(matches!(
            reopened
                .begin_refresh(&config, &credentials.handle, None,)
                .unwrap(),
            RefreshDecision::Missing
        ));

        assert!(!store.finish_refresh(&lease, rotated_tokens(),).unwrap());
    }

    #[test]
    fn refresh_cannot_restore_disabled_user_session() {
        let (dir, store, config) = setup();

        let credentials = near_expiry_session(dir.path(), &store, &config);

        let lease = acquire_refresh(&store, &config, &credentials);

        users::disable_access(dir.path(), &config, SUBJECT).unwrap();

        assert!(!store.finish_refresh(&lease, rotated_tokens(),).unwrap());

        assert!(
            store
                .get_session(&config, &credentials.handle, None,)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn refresh_cannot_restore_logged_out_session() {
        let (dir, store, config) = setup();

        let credentials = near_expiry_session(dir.path(), &store, &config);

        let lease = acquire_refresh(&store, &config, &credentials);

        assert!(
            store
                .revoke_session(&config, &credentials.handle, &credentials.csrf_token,)
                .unwrap()
                .is_some()
        );

        assert!(!store.finish_refresh(&lease, rotated_tokens(),).unwrap());
    }

    #[test]
    fn refresh_requires_matching_csrf_when_supplied() {
        let (dir, store, config) = setup();

        let credentials = near_expiry_session(dir.path(), &store, &config);

        let wrong = random_handle().unwrap();

        assert!(matches!(
            store
                .begin_refresh(&config, &credentials.handle, Some(&wrong),)
                .unwrap(),
            RefreshDecision::Missing
        ));

        assert!(matches!(
            store
                .begin_refresh(&config, &credentials.handle, Some(&credentials.csrf_token),)
                .unwrap(),
            RefreshDecision::Acquired(_)
        ));
    }

    #[test]
    fn session_survives_store_restart() {
        let (dir, store, config) = setup();

        users::grant_access(dir.path(), &config, SUBJECT, "admin").unwrap();

        let credentials = store.create_session(&config, &login()).unwrap().unwrap();

        // Simulate a Workspace restart with the same encryption key.
        let reopened = AuthStore::new(dir.path(), &KEY).unwrap();

        let info = reopened
            .get_session(&config, &credentials.handle, None)
            .unwrap()
            .unwrap();

        assert_eq!(info.user.subject, SUBJECT);
        assert_eq!(info.expires_at_unix, credentials.expires_at_unix);
    }

    #[test]
    fn corrupted_session_is_discarded() {
        let (dir, store, config) = setup();

        users::grant_access(dir.path(), &config, SUBJECT, "user").unwrap();

        let credentials = store.create_session(&config, &login()).unwrap().unwrap();

        let connection = storage::open(dir.path()).unwrap();

        connection
            .execute(
                "UPDATE web_sessions
                 SET encrypted_tokens = X'00'",
                [],
            )
            .unwrap();

        drop(connection);

        assert!(
            store
                .get_session(&config, &credentials.handle, None,)
                .unwrap()
                .is_none()
        );

        let connection = storage::open(dir.path()).unwrap();

        let remaining: i64 = connection
            .query_row("SELECT count(*) FROM web_sessions", [], |row| row.get(0))
            .unwrap();

        assert_eq!(remaining, 0);

        assert!(matches!(
            store
                .begin_refresh(&config, &credentials.handle, None,)
                .unwrap(),
            RefreshDecision::Missing
        ));
    }

    #[test]
    fn rotation_updates_profile_without_changing_role() {
        let (dir, store, config) = setup();

        let credentials = near_expiry_session(dir.path(), &store, &config);

        let lease = acquire_refresh(&store, &config, &credentials);

        assert!(store.finish_refresh(&lease, rotated_tokens(),).unwrap());

        let info = store
            .get_session(&config, &credentials.handle, None)
            .unwrap()
            .unwrap();

        assert_eq!(info.user.username.as_deref(), Some("rotated-user"));
        // The local role is never taken from the SSO profile.
        assert_eq!(info.user.role, "admin");
        assert_eq!(info.expires_at_unix, credentials.expires_at_unix);
    }

    #[test]
    fn revoked_session_exposes_tokens_for_sso_revocation() {
        let (dir, store, config) = setup();

        users::grant_access(dir.path(), &config, SUBJECT, "user").unwrap();

        let credentials = store.create_session(&config, &login()).unwrap().unwrap();

        let tokens = store
            .revoke_session(&config, &credentials.handle, &credentials.csrf_token)
            .unwrap()
            .unwrap();

        assert_eq!(tokens.refresh_token, "test-refresh-token");
    }
}
