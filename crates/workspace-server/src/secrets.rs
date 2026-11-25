use base64::{Engine, engine::general_purpose::STANDARD};
use openidconnect::ClientSecret;
use std::env;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SecretsError {
    #[error("WORKSPACE_SSO_CLIENT_SECRET is missing or invalid")]
    ClientSecret,

    #[error("WORKSPACE_AUTH_KEY must contain a base64-encoded 32-byte key")]
    EncryptionKey,
}

pub struct AuthSecrets {
    client_secret: ClientSecret,
    encryption_key: [u8; 32],
}

impl AuthSecrets {
    pub fn from_env() -> Result<Self, SecretsError> {
        let secret =
            env::var("WORKSPACE_SSO_CLIENT_SECRET").map_err(|_| SecretsError::ClientSecret)?;

        let key = env::var("WORKSPACE_AUTH_KEY").map_err(|_| SecretsError::EncryptionKey)?;

        Self::from_values(&secret, &key)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_secrets() {
        let key = STANDARD.encode([42; 32]);

        let secrets = AuthSecrets::from_values("test-client-secret-123456", &key).unwrap();

        assert_eq!(secrets.encryption_key(), &[42; 32]);
    }

    #[test]
    fn rejects_invalid_client_secret() {
        let key = STANDARD.encode([42; 32]);

        assert!(AuthSecrets::from_values("", &key).is_err());
        assert!(AuthSecrets::from_values("short", &key).is_err());
    }

    #[test]
    fn rejects_invalid_encryption_key() {
        let secret = "test-client-secret-123456";

        assert!(AuthSecrets::from_values(secret, "invalid").is_err());

        assert!(AuthSecrets::from_values(secret, &STANDARD.encode([0; 32]),).is_err());
    }
}
