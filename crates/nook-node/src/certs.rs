//! Keeping this machine's certificate valid, without anybody remembering to.
//!
//! Node certificates last 30 days (`LEAF_VALIDITY_DAYS`) and nothing renewed
//! them automatically — `nook renew` was a command a person had to run. Two
//! consequences, both quiet:
//!
//! 1. A machine enrolled and forgotten drops off the fleet a month later, with
//!    an mTLS handshake failure as the only clue.
//! 2. A CA rotation could never safely complete. Staging distributes the new
//!    authority so nodes learn it *on their next renewal* — but if renewals
//!    only happen when somebody types a command, "their next renewal" is never,
//!    and promoting the new CA locks out the fleet.
//!
//! So renewal has two triggers, and the second is the interesting one:
//!
//! - **A CA this node does not hold.** The control plane advertises what the
//!   tenant trusts; if that includes something missing from our bundle, renew
//!   NOW. Staging a CA therefore sweeps the fleet within minutes rather than
//!   over a month, which is what makes promoting it safe soon after.
//! - **Expiry inside a week.** The ordinary case, with enough margin that a
//!   node offline for a few days still comes back to a working certificate.

use chrono::{DateTime, Duration, Utc};

/// Renew this far ahead of expiry.
///
/// Seven days against a thirty-day certificate: long enough that a machine
/// asleep over a long weekend, or a control plane down for a day, still has
/// room to recover, and short enough that certificates are not being reissued
/// constantly.
pub const RENEW_WITHIN_DAYS: i64 = 7;

/// Why a renewal is happening. Carried into the log line, because "the node
/// renewed" is much less useful than which of these it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The tenant trusts a CA we do not hold — almost always a staged
    /// rotation. Renewing fetches the new trust bundle.
    NewCertificateAuthority,
    /// Ordinary expiry.
    Expiring,
    /// We do not know when ours expires. Treated as "renew": a node with an
    /// unreadable identity is one handshake away from being offline anyway,
    /// and a spurious renewal costs one request.
    Unknown,
}

impl Reason {
    pub fn why(&self) -> &'static str {
        match self {
            Reason::NewCertificateAuthority => {
                "this tenant trusts a certificate authority we do not hold"
            }
            Reason::Expiring => "our certificate expires soon",
            Reason::Unknown => "we cannot tell when our certificate expires",
        }
    }
}

/// Should this node renew?
///
/// Pure, so the policy can be tested without a control plane, a clock or a
/// filesystem — which matters because the failure modes here are all about
/// timing and every one of them is invisible until a month has passed.
pub fn should_renew(
    now: DateTime<Utc>,
    not_after: Option<DateTime<Utc>>,
    trusted_by_server: &[String],
    held_locally: &[String],
) -> Option<Reason> {
    // A CA we have never seen means a rotation is in progress. This comes
    // first: it is time-critical in a way expiry is not, because the operator
    // is waiting to promote and cannot until the fleet has caught up.
    //
    // Only the server's list matters — holding a CA the server has dropped is
    // ordinary during a retirement and is not a reason to do anything.
    if trusted_by_server
        .iter()
        .any(|fp| !held_locally.iter().any(|held| same(held, fp)))
    {
        return Some(Reason::NewCertificateAuthority);
    }

    match not_after {
        None => Some(Reason::Unknown),
        Some(exp) if exp - now <= Duration::days(RENEW_WITHIN_DAYS) => Some(Reason::Expiring),
        Some(_) => None,
    }
}

/// Fingerprints are compared case-insensitively and without separators, since
/// they travel as hex from several places and one of them may add colons.
fn same(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect::<String>()
    };
    !a.is_empty() && norm(a) == norm(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(days: i64) -> DateTime<Utc> {
        Utc::now() + Duration::days(days)
    }

    /// The ordinary case: renew inside the window, not before.
    #[test]
    fn renews_only_as_expiry_approaches() {
        let now = Utc::now();
        let fps = vec!["aa".to_string()];

        assert_eq!(should_renew(now, Some(t(30)), &fps, &fps), None);
        assert_eq!(should_renew(now, Some(t(8)), &fps, &fps), None);
        assert_eq!(
            should_renew(now, Some(t(6)), &fps, &fps),
            Some(Reason::Expiring)
        );
        // Already expired is still "renew", not "give up".
        assert_eq!(
            should_renew(now, Some(t(-1)), &fps, &fps),
            Some(Reason::Expiring)
        );
    }

    /// The trigger that makes rotation possible: a staged CA sweeps the fleet
    /// immediately, rather than waiting up to thirty days for expiry.
    #[test]
    fn a_staged_ca_renews_immediately_however_fresh_the_certificate_is() {
        let now = Utc::now();
        let held = vec!["aa".to_string()];
        let server = vec!["aa".to_string(), "bb".to_string()];

        assert_eq!(
            should_renew(now, Some(t(29)), &server, &held),
            Some(Reason::NewCertificateAuthority),
            "a brand-new certificate must still renew when a CA is staged — \
             otherwise promoting it locks this node out"
        );
    }

    /// A retiring CA is the mirror image and must NOT trigger anything: the
    /// server dropping one we still hold is what retirement looks like.
    #[test]
    fn holding_a_ca_the_server_has_dropped_is_not_a_reason() {
        let now = Utc::now();
        let held = vec!["aa".to_string(), "old".to_string()];
        let server = vec!["aa".to_string()];
        assert_eq!(should_renew(now, Some(t(20)), &server, &held), None);
    }

    /// Unknown expiry fails toward renewing. A node that cannot read its own
    /// identity is already in trouble; one extra request is the cheap side of
    /// that trade.
    #[test]
    fn an_unknown_expiry_renews() {
        let fps = vec!["aa".to_string()];
        assert_eq!(
            should_renew(Utc::now(), None, &fps, &fps),
            Some(Reason::Unknown)
        );
    }

    #[test]
    fn fingerprints_compare_regardless_of_formatting() {
        let now = Utc::now();
        let held = vec!["AA:BB:CC".to_string()];
        let server = vec!["aabbcc".to_string()];
        assert_eq!(
            should_renew(now, Some(t(20)), &server, &held),
            None,
            "the same CA written two ways must not look like a new one"
        );
    }

    /// A server that reports nothing must not cause a renewal storm — an empty
    /// list is "I have nothing to say", not "you hold too much".
    #[test]
    fn an_empty_server_list_changes_nothing() {
        let now = Utc::now();
        let held = vec!["aa".to_string()];
        assert_eq!(should_renew(now, Some(t(20)), &[], &held), None);
    }
}
