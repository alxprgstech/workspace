use crate::{
    crypto::{CryptoError, SecretCipher, hash_handle, random_handle, random_lease_id},
    sso::{self, AuthorizationRequest},
    storage::{self, StorageError},
};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;
use workspace_core::config::SsoConfig;

#[path = "sessions.rs"]
mod sessions;

pub use sessions::{RefreshDecision, RefreshLease, SessionCredentials, SessionInfo};

const FLOW_TTL_SECONDS: i64 = 600;

#[derive(Debug, Error)]
pub enum AuthStoreError {
    #[error("user operation failed: {0}")]
    User(#[from] crate::users::UserError),

    #[error("authentication storage error: {0}")]
    Storage(#[from] StorageError),

    #[error("authentication database operation failed")]
    Sqlite(#[from] rusqlite::Error),

    #[error("authentication cryptographic operation failed: {0}")]
    Crypto(#[from] CryptoError),

    #[error("invalid authentication flow")]
    InvalidFlow,

    #[error("invalid encrypted authentication payload")]
    InvalidPayload,
}

#[derive(Serialize, Deserialize)]
pub struct FlowSecrets {
    pub nonce: String,
    pub verifier: String,
}

pub struct AuthStore {
    data_dir: PathBuf,
    cipher: SecretCipher,
}

fn flow_aad(handle_hash: &[u8; 32], state_hash: &[u8; 32], config: &SsoConfig) -> Vec<u8> {
    let mut aad = b"workspace:auth-flow:v1\0".to_vec();

    aad.extend_from_slice(handle_hash);
    aad.extend_from_slice(state_hash);
    aad.extend_from_slice(config.client_id.as_bytes());
    aad.push(0);
    aad.extend_from_slice(config.redirect_uri.as_bytes());

    aad
}

impl AuthStore {
    pub fn new(data_dir: &Path, key: &[u8; 32]) -> Result<Self, AuthStoreError> {
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            cipher: SecretCipher::new(key)?,
        })
    }

    pub fn create_flow(
        &self,
        config: &SsoConfig,
        request: &AuthorizationRequest,
    ) -> Result<String, AuthStoreError> {
        sso::validate_config(config).map_err(|_| AuthStoreError::InvalidFlow)?;

        let handle = random_handle()?;

        let handle_hash = hash_handle(&handle);
        let state_hash = hash_handle(request.state.secret());

        let secrets = FlowSecrets {
            nonce: request.nonce.secret().to_owned(),
            verifier: request.verifier.secret().to_owned(),
        };

        let plaintext = serde_json::to_vec(&secrets).map_err(|_| AuthStoreError::InvalidPayload)?;

        let encrypted = self
            .cipher
            .seal(&plaintext, &flow_aad(&handle_hash, &state_hash, config))?;

        let mut connection = storage::open(&self.data_dir)?;

        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        tx.execute(
            "DELETE FROM auth_flows
             WHERE expires_at_unix <= unixepoch()",
            [],
        )?;

        tx.execute(
            "INSERT INTO auth_flows (
                handle_hash,
                state_hash,
                client_id,
                redirect_uri,
                encrypted_secrets,
                created_at_unix,
                expires_at_unix
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                unixepoch(), unixepoch() + ?6
             )",
            params![
                &handle_hash[..],
                &state_hash[..],
                &config.client_id,
                &config.redirect_uri,
                encrypted,
                FLOW_TTL_SECONDS,
            ],
        )?;

        tx.commit()?;

        Ok(handle)
    }

    pub fn consume_flow(
        &self,
        config: &SsoConfig,
        handle: &str,
        state: &str,
    ) -> Result<Option<FlowSecrets>, AuthStoreError> {
        sso::validate_config(config).map_err(|_| AuthStoreError::InvalidFlow)?;

        if handle.len() != 43
            || state.is_empty()
            || state.len() > 128
            || !handle
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Ok(None);
        }

        let handle_hash = hash_handle(handle);
        let state_hash = hash_handle(state);

        let mut connection = storage::open(&self.data_dir)?;

        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let encrypted: Option<Vec<u8>> = tx
            .query_row(
                "DELETE FROM auth_flows
             WHERE handle_hash = ?1
               AND state_hash = ?2
               AND client_id = ?3
               AND redirect_uri = ?4
               AND expires_at_unix > unixepoch()
             RETURNING encrypted_secrets",
                params![
                    &handle_hash[..],
                    &state_hash[..],
                    &config.client_id,
                    &config.redirect_uri,
                ],
                |row| row.get(0),
            )
            .optional()?;

        tx.commit()?;

        let Some(encrypted) = encrypted else {
            return Ok(None);
        };

        let plaintext = self
            .cipher
            .open(&encrypted, &flow_aad(&handle_hash, &state_hash, config))?;

        let secrets =
            serde_json::from_slice(&plaintext).map_err(|_| AuthStoreError::InvalidPayload)?;

        Ok(Some(secrets))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [42; 32];

    fn config() -> SsoConfig {
        SsoConfig {
            issuer: "https://auth.alxprgs.tech".to_owned(),
            client_id: "test-client".to_owned(),
            redirect_uri: "https://workspace.example.com/auth/callback".to_owned(),
            trusted_proxies: Vec::new(),
        }
    }

    #[test]
    fn flow_is_one_use_and_bound_to_state() {
        let dir = tempfile::tempdir().unwrap();
        storage::initialize(dir.path()).unwrap();

        let store = AuthStore::new(dir.path(), &KEY).unwrap();
        let config = config();
        let request = sso::start_authorization(&config).unwrap();

        let handle = store.create_flow(&config, &request).unwrap();

        assert!(
            store
                .consume_flow(&config, &handle, "wrong-state")
                .unwrap()
                .is_none()
        );

        let result = store
            .consume_flow(&config, &handle, request.state.secret())
            .unwrap()
            .unwrap();

        assert_eq!(result.nonce, *request.nonce.secret());
        assert_eq!(result.verifier, *request.verifier.secret());

        assert!(
            store
                .consume_flow(&config, &handle, request.state.secret(),)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn flow_is_bound_to_client() {
        let dir = tempfile::tempdir().unwrap();
        storage::initialize(dir.path()).unwrap();

        let store = AuthStore::new(dir.path(), &KEY).unwrap();
        let config = config();
        let request = sso::start_authorization(&config).unwrap();

        let handle = store.create_flow(&config, &request).unwrap();

        let mut other = config.clone();
        other.client_id = "other-client".to_owned();

        assert!(
            store
                .consume_flow(&other, &handle, request.state.secret(),)
                .unwrap()
                .is_none()
        );

        assert!(
            store
                .consume_flow(&config, &handle, request.state.secret(),)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn expired_flow_cannot_be_consumed() {
        let dir = tempfile::tempdir().unwrap();
        storage::initialize(dir.path()).unwrap();

        let store = AuthStore::new(dir.path(), &KEY).unwrap();
        let config = config();
        let request = sso::start_authorization(&config).unwrap();

        let handle = store.create_flow(&config, &request).unwrap();

        let connection = storage::open(dir.path()).unwrap();

        connection
            .execute(
                "UPDATE auth_flows
             SET created_at_unix = unixepoch() - 601,
                 expires_at_unix = unixepoch() - 1",
                [],
            )
            .unwrap();

        assert!(
            store
                .consume_flow(&config, &handle, request.state.secret(),)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn flow_secrets_are_encrypted() {
        let dir = tempfile::tempdir().unwrap();
        storage::initialize(dir.path()).unwrap();

        let store = AuthStore::new(dir.path(), &KEY).unwrap();
        let config = config();
        let request = sso::start_authorization(&config).unwrap();

        store.create_flow(&config, &request).unwrap();

        let connection = storage::open(dir.path()).unwrap();

        let encrypted: Vec<u8> = connection
            .query_row("SELECT encrypted_secrets FROM auth_flows", [], |row| {
                row.get(0)
            })
            .unwrap();

        let verifier = request.verifier.secret().as_bytes();

        assert!(
            !encrypted
                .windows(verifier.len())
                .any(|window| window == verifier)
        );
    }

    #[test]
    fn concurrent_callbacks_consume_flow_only_once() {
        let dir = tempfile::tempdir().unwrap();
        storage::initialize(dir.path()).unwrap();

        let store = AuthStore::new(dir.path(), &KEY).unwrap();
        let config = config();
        let request = sso::start_authorization(&config).unwrap();

        let handle = store.create_flow(&config, &request).unwrap();

        let state = request.state.secret().to_owned();

        let successes = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..2)
                .map(|_| {
                    let path = dir.path();
                    let config = &config;
                    let handle = &handle;
                    let state = &state;

                    scope.spawn(move || {
                        AuthStore::new(path, &KEY)
                            .unwrap()
                            .consume_flow(config, handle, state)
                            .unwrap()
                            .is_some()
                    })
                })
                .collect();

            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .filter(|success| *success)
                .count()
        });

        assert_eq!(successes, 1);
    }
}
