use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{
    aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey},
    digest,
    rand::{SecureRandom, SystemRandom},
};
use thiserror::Error;

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const MAX_SECRET_BYTES: usize = 65_536;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("secure random generation failed")]
    Random,

    #[error("invalid encryption key")]
    InvalidKey,

    #[error("secret encryption failed")]
    Encryption,

    #[error("secret decryption failed")]
    Decryption,

    #[error("invalid secret size")]
    InvalidSize,
}

pub struct SecretCipher {
    key: LessSafeKey,
}

pub fn random_handle() -> Result<String, CryptoError> {
    let mut bytes = [0_u8; 32];

    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| CryptoError::Random)?;

    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub fn hash_handle(value: &str) -> [u8; 32] {
    let digest = digest::digest(&digest::SHA256, value.as_bytes());

    let mut hash = [0_u8; 32];
    hash.copy_from_slice(digest.as_ref());
    hash
}

impl SecretCipher {
    pub fn new(key: &[u8; 32]) -> Result<Self, CryptoError> {
        let key = UnboundKey::new(&AES_256_GCM, key).map_err(|_| CryptoError::InvalidKey)?;

        Ok(Self {
            key: LessSafeKey::new(key),
        })
    }

    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if plaintext.len() > MAX_SECRET_BYTES {
            return Err(CryptoError::InvalidSize);
        }

        let mut nonce = [0_u8; NONCE_LEN];

        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| CryptoError::Random)?;

        let mut ciphertext = plaintext.to_vec();

        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut ciphertext,
            )
            .map_err(|_| CryptoError::Encryption)?;

        let mut output = nonce.to_vec();
        output.extend_from_slice(&ciphertext);

        Ok(output)
    }

    pub fn open(&self, encrypted: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if encrypted.len() < NONCE_LEN + TAG_LEN
            || encrypted.len() > NONCE_LEN + TAG_LEN + MAX_SECRET_BYTES
        {
            return Err(CryptoError::InvalidSize);
        }

        let nonce: [u8; NONCE_LEN] = encrypted[..NONCE_LEN]
            .try_into()
            .map_err(|_| CryptoError::Decryption)?;

        let mut ciphertext = encrypted[NONCE_LEN..].to_vec();

        let plaintext = self
            .key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut ciphertext,
            )
            .map_err(|_| CryptoError::Decryption)?;

        Ok(plaintext.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cipher_binds_context_and_rejects_tampering() {
        let cipher = SecretCipher::new(&[42; 32]).unwrap();

        let first = cipher.seal(b"test-secret", b"flow-a").unwrap();
        let second = cipher.seal(b"test-secret", b"flow-a").unwrap();

        assert_ne!(first, second);

        assert_eq!(cipher.open(&first, b"flow-a").unwrap(), b"test-secret");

        assert!(cipher.open(&first, b"flow-b").is_err());

        let wrong_key = SecretCipher::new(&[43; 32]).unwrap();
        assert!(wrong_key.open(&first, b"flow-a").is_err());

        let mut corrupted = first;
        let last = corrupted.len() - 1;
        corrupted[last] ^= 1;

        assert!(cipher.open(&corrupted, b"flow-a").is_err());
    }
}
