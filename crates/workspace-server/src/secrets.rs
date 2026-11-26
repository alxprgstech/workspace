use base64::{Engine, engine::general_purpose::STANDARD};
use openidconnect::ClientSecret;
use serde::{Deserialize, Serialize};
use std::{
    env, fs, io,
    path::{Path, PathBuf},
};
use thiserror::Error;

/// Where the installation keeps the DPAPI-protected copy of the two secrets.
///
/// The file sits next to the database, which already holds encrypted OAuth
/// material. It is encrypted for the current Windows user; the process never
/// logs or prints its contents.
const SECRET_FILE: &str = "secrets.dat";
const SECRET_FORMAT: u32 = 1;

#[derive(Debug, Error)]
pub enum SecretsError {
    #[error("WORKSPACE_SSO_CLIENT_SECRET is missing or invalid")]
    ClientSecret,

    #[error("WORKSPACE_AUTH_KEY must contain a base64-encoded 32-byte key")]
    EncryptionKey,

    #[error("only one of WORKSPACE_SSO_CLIENT_SECRET and WORKSPACE_AUTH_KEY is set")]
    IncompleteEnvironment,

    #[error(
        "installation secrets are not available: set WORKSPACE_SSO_CLIENT_SECRET and WORKSPACE_AUTH_KEY once"
    )]
    NotAvailable,

    #[error("installation secrets cannot be read: {0}")]
    StoreUnreadable(String),

    #[error("installation secrets cannot be persisted: {0}")]
    StoreUnwritable(String),
}

pub struct AuthSecrets {
    client_secret: ClientSecret,
    encryption_key: [u8; 32],
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretFile {
    format: u32,
    client_secret: String,
    auth_key: String,
}

impl AuthSecrets {
    /// Read the secrets from the process environment only.
    pub fn from_env() -> Result<Self, SecretsError> {
        let secret =
            env::var("WORKSPACE_SSO_CLIENT_SECRET").map_err(|_| SecretsError::ClientSecret)?;

        let key = env::var("WORKSPACE_AUTH_KEY").map_err(|_| SecretsError::EncryptionKey)?;

        Self::from_values(&secret, &key)
    }

    /// Load the secrets without prompting.
    ///
    /// A complete environment wins and is persisted for the next update. When
    /// the environment is empty the DPAPI-protected copy written by an earlier
    /// start is used, so an update never asks for the same secrets again. A
    /// partial environment is a configuration error and never silently mixes
    /// sources.
    pub fn load(data_dir: &Path) -> Result<Self, SecretsError> {
        let secret = env::var("WORKSPACE_SSO_CLIENT_SECRET").ok();
        let key = env::var("WORKSPACE_AUTH_KEY").ok();

        match (secret, key) {
            (Some(secret), Some(key)) => {
                let secrets = Self::from_values(&secret, &key)?;

                if let Err(error) = secrets.persist(data_dir) {
                    // Persisting is best effort: an installation that cannot
                    // write the store still starts normally with its
                    // environment. The error carries no secret material.
                    tracing::warn!(%error, "Cannot persist installation secrets");
                }

                Ok(secrets)
            }
            (None, None) => Self::from_store(data_dir),
            _ => Err(SecretsError::IncompleteEnvironment),
        }
    }

    pub fn from_values(client_secret: &str, encoded_key: &str) -> Result<Self, SecretsError> {
        if !(16..=128).contains(&client_secret.len())
            || !client_secret
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(SecretsError::ClientSecret);
        }

        if encoded_key.len() != 44 {
            return Err(SecretsError::EncryptionKey);
        }

        let key: [u8; 32] = STANDARD
            .decode(encoded_key)
            .map_err(|_| SecretsError::EncryptionKey)?
            .try_into()
            .map_err(|_| SecretsError::EncryptionKey)?;

        if key == [0; 32] {
            return Err(SecretsError::EncryptionKey);
        }

        Ok(Self {
            client_secret: ClientSecret::new(client_secret.to_owned()),
            encryption_key: key,
        })
    }

    pub fn client_secret(&self) -> &ClientSecret {
        &self.client_secret
    }

    pub fn encryption_key(&self) -> &[u8; 32] {
        &self.encryption_key
    }

    /// Raw value for starting a child process. Never log this.
    pub fn client_secret_value(&self) -> &str {
        self.client_secret.secret()
    }

    /// Base64 form expected by `WORKSPACE_AUTH_KEY`. Never log this.
    pub fn auth_key_base64(&self) -> String {
        STANDARD.encode(self.encryption_key)
    }

    /// Protected copy of the secrets for this Windows user.
    pub fn secret_file(data_dir: &Path) -> PathBuf {
        data_dir.join("state").join(SECRET_FILE)
    }

    /// Write the secrets for the next process. No-op on unsupported platforms
    /// (Linux deployments provide the environment through their service unit).
    pub fn persist(&self, data_dir: &Path) -> Result<(), SecretsError> {
        if !workspace_os::dpapi::is_supported() {
            return Ok(());
        }

        let encoded_key = STANDARD.encode(self.encryption_key);

        let payload = serde_json::to_vec(&SecretFile {
            format: SECRET_FORMAT,
            client_secret: self.client_secret.secret().to_owned(),
            auth_key: encoded_key,
        })
        .map_err(|error| SecretsError::StoreUnwritable(error.to_string()))?;

        let protected = workspace_os::dpapi::protect(&payload)
            .map_err(|error| SecretsError::StoreUnwritable(error.to_string()))?;

        let directory = data_dir.join("state");

        fs::create_dir_all(&directory)
            .map_err(|error| SecretsError::StoreUnwritable(error.to_string()))?;

        let target = Self::secret_file(data_dir);
        let temporary = target.with_extension("dat.tmp");

        fs::write(&temporary, &protected)
            .map_err(|error| SecretsError::StoreUnwritable(error.to_string()))?;

        match fs::rename(&temporary, &target) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                Err(SecretsError::StoreUnwritable(error.to_string()))
            }
        }
    }

    fn from_store(data_dir: &Path) -> Result<Self, SecretsError> {
        let path = Self::secret_file(data_dir);

        let protected = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(SecretsError::NotAvailable);
            }
            Err(error) => return Err(SecretsError::StoreUnreadable(error.to_string())),
        };

        let payload = workspace_os::dpapi::unprotect(&protected)
            .map_err(|error| SecretsError::StoreUnreadable(error.to_string()))?;

        let file: SecretFile = serde_json::from_slice(&payload)
            .map_err(|_| SecretsError::StoreUnreadable("invalid secret file".into()))?;

        if file.format != SECRET_FORMAT {
            return Err(SecretsError::StoreUnreadable(
                "unsupported secret file format".into(),
            ));
        }

        let secrets = Self::from_values(&file.client_secret, &file.auth_key)
            .map_err(|error| SecretsError::StoreUnreadable(error.to_string()))?;

        Ok(secrets)
    }
}

impl std::fmt::Debug for SecretFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render the values, even if a future caller debug-prints the
        // struct by accident.
        formatter
            .debug_struct("SecretFile")
            .field("format", &self.format)
            .field("client_secret", &"<redacted>")
            .field("auth_key", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;

    fn key() -> String {
        STANDARD.encode([42u8; 32])
    }

    #[test]
    fn accepts_valid_secrets() {
        let secrets = AuthSecrets::from_values("test-client-secret-123456", &key()).unwrap();

        assert_eq!(secrets.encryption_key(), &[42u8; 32]);
    }

    #[test]
    fn rejects_invalid_client_secret() {
        assert!(AuthSecrets::from_values("", &key()).is_err());
        assert!(AuthSecrets::from_values("short", &key()).is_err());
    }

    #[test]
    fn rejects_invalid_encryption_key() {
        let secret = "test-client-secret-123456";

        assert!(AuthSecrets::from_values(secret, "invalid").is_err());
        assert!(AuthSecrets::from_values(secret, &STANDARD.encode([0; 32])).is_err());
    }

    #[test]
    fn missing_store_is_reported_without_prompting() {
        let directory = tempfile::tempdir().unwrap();

        assert!(matches!(
            AuthSecrets::from_store(directory.path()),
            Err(SecretsError::NotAvailable)
        ));
    }

    #[test]
    fn debug_never_renders_secret_values() {
        let file = SecretFile {
            format: SECRET_FORMAT,
            client_secret: "super-secret-value".into(),
            auth_key: key(),
        };

        let rendered = format!("{file:?}");

        assert!(!rendered.contains("super-secret-value"));
    }

    #[cfg(windows)]
    #[test]
    fn persist_and_load_roundtrip() {
        let directory = tempfile::tempdir().unwrap();
        let secrets = AuthSecrets::from_values("test-client-secret-123456", &key()).unwrap();

        secrets.persist(directory.path()).unwrap();

        assert!(AuthSecrets::secret_file(directory.path()).is_file());

        let loaded = AuthSecrets::from_store(directory.path()).unwrap();

        assert_eq!(loaded.encryption_key(), secrets.encryption_key());

        let raw = fs::read(AuthSecrets::secret_file(directory.path())).unwrap();

        assert!(
            !raw.windows(5).any(|window| window == b"test-"),
            "the protected file must not contain the plaintext secret"
        );
    }
}
