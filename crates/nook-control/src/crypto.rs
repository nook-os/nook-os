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

/// Passphrase-sealed payloads.
///
/// The vault's app key alone must not be enough to read a secret: a database
/// dump plus `SECRETS_KEY` is a realistic breach, and it would otherwise hand
/// over every tenant's credentials. When a secret carries a passphrase, the
/// content is sealed with a key derived from that passphrase *before* the app
/// key wraps it — so an attacker needs the dump, the app key, and something
/// the server never stores.
///
/// The KDF is PBKDF2-HMAC-SHA256 with a high iteration count. Argon2id would
/// resist GPUs better and is the documented target in
/// `docs/secrets-encryption.md`; this keeps the dependency surface unchanged
/// while closing the "app key is enough" hole today.
pub struct Sealed {
    pub salt: Vec<u8>,
    /// Proves a passphrase is right without trying to decrypt.
    pub verifier: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

const KDF_ITERATIONS: u32 = 210_000;

fn derive(passphrase: &str, salt: &[u8]) -> [u8; 32] {
    use hmac::Mac as _;
    type Hmac = hmac::Hmac<sha2::Sha256>;

    let mac_for = |data: &[u8]| {
        let mut mac = <Hmac as hmac::Mac>::new_from_slice(passphrase.as_bytes())
            .expect("hmac accepts any key length");
        mac.update(data);
        mac.finalize().into_bytes()
    };

    // PBKDF2: U1 = PRF(pass, salt || INT(1)); Un = PRF(pass, Un-1); DK = ⊕Un.
    let mut block = Vec::with_capacity(salt.len() + 4);
    block.extend_from_slice(salt);
    block.extend_from_slice(&1u32.to_be_bytes());

    let mut u = mac_for(&block);
    let mut out = u;
    for _ in 1..KDF_ITERATIONS {
        u = mac_for(&u);
        for (o, x) in out.iter_mut().zip(u.iter()) {
            *o ^= x;
        }
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&out[..32]);
    key
}

/// A one-way check value for the derived key — never the key itself.
fn verifier_of(key: &[u8; 32]) -> Vec<u8> {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(b"nook-secret-verifier:");
    h.update(key);
    h.finalize().to_vec()
}

/// Seal plaintext under a passphrase.
pub fn seal_with_passphrase(plaintext: &[u8], passphrase: &str) -> Result<Sealed> {
    use aes_gcm::aead::{rand_core::RngCore, Aead, KeyInit, OsRng};

    let mut salt = vec![0u8; 16];
    OsRng.fill_bytes(&mut salt);
    let key = derive(passphrase, &salt);
    let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let body = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;
    let mut ciphertext = nonce.to_vec();
    ciphertext.extend_from_slice(&body);
    Ok(Sealed {
        salt,
        verifier: verifier_of(&key),
        ciphertext,
    })
}

/// Open a passphrase-sealed payload. A wrong passphrase is reported as such
/// rather than as a decryption failure.
pub fn open_with_passphrase(
    ciphertext: &[u8],
    salt: &[u8],
    verifier: &[u8],
    passphrase: &str,
) -> Result<Vec<u8>> {
    use aes_gcm::aead::{Aead, KeyInit};

    if ciphertext.len() < 13 {
        anyhow::bail!("sealed secret is truncated");
    }
    let key = derive(passphrase, salt);
    if verifier_of(&key) != verifier {
        anyhow::bail!("wrong passphrase");
    }
    let (nonce, body) = ciphertext.split_at(12);
    let nonce: [u8; 12] = nonce.try_into().expect("split_at(12) yields 12 bytes");
    Aes256Gcm::new(&Key::<Aes256Gcm>::from(key))
        .decrypt(&Nonce::from(nonce), body)
        .map_err(|_| anyhow::anyhow!("decryption failed"))
}

#[cfg(test)]
mod passphrase_tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_a_wrong_passphrase() {
        let sealed = seal_with_passphrase(b"API_KEY=hunter2", "correct horse").unwrap();
        let opened = open_with_passphrase(
            &sealed.ciphertext,
            &sealed.salt,
            &sealed.verifier,
            "correct horse",
        )
        .unwrap();
        assert_eq!(opened, b"API_KEY=hunter2");

        let err = open_with_passphrase(
            &sealed.ciphertext,
            &sealed.salt,
            &sealed.verifier,
            "wrong horse",
        )
        .unwrap_err();
        assert!(err.to_string().contains("wrong passphrase"));
    }

    #[test]
    fn the_app_key_alone_cannot_read_it() {
        // What a database dump + SECRETS_KEY would yield: the app-key layer
        // unwraps to ciphertext that is still sealed by the passphrase.
        let vault = Vault::from_env("test-secret-test-secret-test-secret").unwrap();
        let sealed = seal_with_passphrase(b"DB_URL=postgres://prod", "pass phrase").unwrap();
        let at_rest = vault.encrypt(&sealed.ciphertext).unwrap();

        let unwrapped = vault.decrypt(&at_rest).unwrap();
        assert_eq!(unwrapped, sealed.ciphertext);
        assert!(!String::from_utf8_lossy(&unwrapped).contains("postgres://prod"));
    }

    #[test]
    fn each_seal_uses_a_fresh_salt() {
        let a = seal_with_passphrase(b"x", "p").unwrap();
        let b = seal_with_passphrase(b"x", "p").unwrap();
        assert_ne!(a.salt, b.salt);
        assert_ne!(a.ciphertext, b.ciphertext);
    }
}

/// Salt + verifier for an app password, so the server can reject a wrong one
/// without ever holding the password or the key it derives.
pub fn passphrase_verifier(passphrase: &str) -> (Vec<u8>, Vec<u8>) {
    use aes_gcm::aead::{rand_core::RngCore, OsRng};
    let mut salt = vec![0u8; 16];
    OsRng.fill_bytes(&mut salt);
    let key = derive(passphrase, &salt);
    (salt, verifier_of(&key))
}

pub fn verify_passphrase(passphrase: &str, salt: &[u8], verifier: &[u8]) -> bool {
    verifier_of(&derive(passphrase, salt)) == verifier
}
