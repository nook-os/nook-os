//! Encryption at rest for the vault (git credentials, workspace secrets).
//!
//! AES-256-GCM. The key comes from `SECRETS_KEY` (64 hex chars); in dev it
//! falls back to a key derived from `SESSION_SECRET` so the stack boots with
//! zero extra setup — with a loud warning, because rotating SESSION_SECRET
//! would then orphan stored secrets. Stored format: nonce(12) || ciphertext.

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct Vault {
    cipher: Aes256Gcm,
}

impl Vault {
    pub fn from_env(session_secret: &str) -> Result<Self> {
        let key_bytes: [u8; 32] = match std::env::var("SECRETS_KEY").ok().filter(|v| !v.is_empty())
        {
            Some(hex) => {
                let bytes = hex_decode(hex.trim()).context("SECRETS_KEY must be hex")?;
                bytes
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("SECRETS_KEY must be 64 hex chars (32 bytes)"))?
            }
            None => {
                tracing::warn!(
                    "SECRETS_KEY not set — deriving vault key from SESSION_SECRET (dev only; \
                     set SECRETS_KEY in production)"
                );
                Sha256::digest(format!("nook-vault:{session_secret}").as_bytes()).into()
            }
        };
        let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(key_bytes));
        Ok(Self { cipher })
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext)
            .map_err(|_| anyhow::anyhow!("encryption failed"))?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    pub fn decrypt(&self, stored: &[u8]) -> Result<Vec<u8>> {
        if stored.len() < 13 {
            anyhow::bail!("stored secret too short");
        }
        let (nonce, ciphertext) = stored.split_at(12);
        let nonce: [u8; 12] = nonce.try_into().expect("split_at(12) yields 12 bytes");
        self.cipher
            .decrypt(&Nonce::from(nonce), ciphertext)
            .map_err(|_| anyhow::anyhow!("decryption failed (wrong SECRETS_KEY?)"))
    }

    pub fn decrypt_string(&self, stored: &[u8]) -> Result<String> {
        Ok(String::from_utf8(self.decrypt(stored)?)?)
    }
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        anyhow::bail!("odd hex length");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).context("bad hex"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::Vault;

    #[test]
    fn round_trips_and_uses_unique_nonces() {
        std::env::remove_var("SECRETS_KEY");
        let vault = Vault::from_env("test-secret-test-secret-test-secret").unwrap();
        let a = vault.encrypt(b"API_KEY=hunter2").unwrap();
        let b = vault.encrypt(b"API_KEY=hunter2").unwrap();
        assert_ne!(a, b, "nonces must differ");
        assert_eq!(vault.decrypt(&a).unwrap(), b"API_KEY=hunter2");
        assert_eq!(vault.decrypt_string(&b).unwrap(), "API_KEY=hunter2");
    }

    #[test]
    fn tampering_fails() {
        std::env::remove_var("SECRETS_KEY");
        let vault = Vault::from_env("test-secret-test-secret-test-secret").unwrap();
        let mut stored = vault.encrypt(b"data").unwrap();
        let last = stored.len() - 1;
        stored[last] ^= 0xff;
        assert!(vault.decrypt(&stored).is_err());
    }
}
