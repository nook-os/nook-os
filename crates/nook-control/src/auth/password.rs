//! Password hashing for local accounts.
//!
//! Argon2id, with a PHC string that carries its own parameters — so raising
//! the cost later is a code change, and every hash written before it still
//! verifies. The alternative, a bare hash plus parameters held somewhere else,
//! means you can never change them.

use anyhow::{bail, Result};
// OsRng comes from password-hash's own rand_core, not the workspace `rand`:
// the two are different major versions, and the traits do not unify.
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

/// Shortest password we will store.
///
/// Length is the only thing that reliably helps, so this is a floor rather
/// than a character-class rule — "must contain a symbol" pushes people to
/// `Password1!` and buys nothing.
pub const MIN_LENGTH: usize = 12;

/// Refuse what we cannot store safely, and say why.
pub fn check_strength(password: &str) -> Result<()> {
    let chars = password.chars().count();
    if chars < MIN_LENGTH {
        bail!("password must be at least {MIN_LENGTH} characters (this one is {chars})");
    }
    // Argon2 itself is fine with long inputs, but an unbounded password is an
    // unbounded amount of hashing work an anonymous caller can ask for.
    if password.len() > 1024 {
        bail!("password must be at most 1024 bytes");
    }
    Ok(())
}

/// Hash a password into a PHC string suitable for storing.
pub fn hash(password: &str) -> Result<String> {
    check_strength(password)?;
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("cannot hash password: {e}"))
}

/// Check a password against a stored PHC string.
///
/// A malformed stored hash verifies as `false` rather than erroring: a row
/// corrupted by hand should fail the login, not hand the caller a different
/// response that distinguishes it from a wrong password.
pub fn verify(password: &str, stored: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Spend the same work as a real verification, for a user that does not exist.
///
/// Without this, "no such user" returns in microseconds while a real account
/// takes the full Argon2 cost — which turns the login endpoint into a way to
/// enumerate who has an account here. Hashing against a fixed dummy makes both
/// paths cost the same.
pub fn waste_time() {
    // A pre-computed hash of a value nobody can log in with.
    const DUMMY: &str = "$argon2id$v=19$m=19456,t=2,p=1\
        $c29tZXNhbHRzb21lc2FsdA$Ru7bTFbEcvzO9zTBd2j1w1SzOgvJj1vXlPRVwkKMQ3Y";
    let _ = verify("not-the-password", DUMMY);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_verifies_against_its_own_hash() {
        let h = hash("correct horse battery staple").unwrap();
        assert!(verify("correct horse battery staple", &h));
        assert!(!verify("Correct horse battery staple", &h));
        assert!(!verify("", &h));
    }

    /// Two hashes of the same password must differ, or the salt is not doing
    /// its job and identical passwords become visible across accounts.
    #[test]
    fn hashing_is_salted() {
        let a = hash("correct horse battery staple").unwrap();
        let b = hash("correct horse battery staple").unwrap();
        assert_ne!(a, b);
        assert!(verify("correct horse battery staple", &a));
        assert!(verify("correct horse battery staple", &b));
    }

    /// The stored form must carry its parameters, so the cost can be raised
    /// later without invalidating everything written before.
    #[test]
    fn the_stored_form_is_a_phc_string() {
        let h = hash("correct horse battery staple").unwrap();
        assert!(h.starts_with("$argon2id$"), "{h}");
        assert!(h.contains("m="), "no memory parameter: {h}");
        assert!(h.contains("t="), "no time parameter: {h}");
    }

    #[test]
    fn short_passwords_are_refused_with_a_useful_message() {
        let e = hash("short").unwrap_err().to_string();
        assert!(e.contains("at least 12"), "{e}");
        assert!(
            e.contains("is 5"),
            "should say how long it actually was: {e}"
        );
        assert!(check_strength(&"a".repeat(12)).is_ok());
    }

    /// An unbounded password is unbounded hashing work for an anonymous
    /// caller — a denial-of-service handed over politely.
    #[test]
    fn absurdly_long_passwords_are_refused() {
        assert!(hash(&"a".repeat(5000)).is_err());
    }

    /// A corrupted row must fail closed, and must not be distinguishable from
    /// an ordinary wrong password.
    #[test]
    fn a_malformed_stored_hash_never_verifies() {
        assert!(!verify("anything", "not-a-phc-string"));
        assert!(!verify("anything", ""));
        assert!(!verify("anything", "$argon2id$garbage"));
    }

    /// The dummy used for timing equalisation has to be a hash we can actually
    /// run — if it were malformed, `verify` would bail early and the timing
    /// difference it exists to hide would come straight back.
    #[test]
    fn the_timing_dummy_is_a_real_hash() {
        const DUMMY: &str = "$argon2id$v=19$m=19456,t=2,p=1\
            $c29tZXNhbHRzb21lc2FsdA$Ru7bTFbEcvzO9zTBd2j1w1SzOgvJj1vXlPRVwkKMQ3Y";
        assert!(
            PasswordHash::new(DUMMY).is_ok(),
            "the dummy hash must parse, or waste_time() returns immediately"
        );
        waste_time();
    }
}
