//! GitHub's inbound webhook: its signature scheme, and what a delivery means
//! (MAIN-554).
//!
//! ## Why this is not `notify::sign`
//!
//! That function signs what NookOS *sends*: `t=<unix>,v1=<hex of "ts.body">`,
//! with the timestamp inside the signed material so a receiver can reject a
//! replay. GitHub signs what it sends with neither a timestamp nor that
//! framing — `sha256=<hex hmac of the raw body>` — and we do not get to choose.
//! So this is a sibling of that function, not a reuse of it; sharing one would
//! mean a parameter deciding which of two schemes to speak, on the code path
//! where getting it wrong means accepting a forgery.
//!
//! ## The RAW body, not the parsed one
//!
//! The HMAC is over the exact bytes GitHub sent. Serializing a
//! `serde_json::Value` back out produces different bytes — key order, spacing,
//! number formatting — so the handler verifies before it parses and never the
//! other way round.

use hmac::{Hmac, Mac};
use rand::distr::Alphanumeric;
use rand::Rng;
use sha2::Sha256;

use nook_types::WorkspaceId;

/// The header GitHub carries the signature in.
pub const SIGNATURE_HEADER: &str = "x-hub-signature-256";
/// The header naming the event, and the one naming the delivery.
pub const EVENT_HEADER: &str = "x-github-event";
pub const DELIVERY_HEADER: &str = "x-github-delivery";

/// The events an operator is told to subscribe (AC-8), and the ones the
/// children of this card will grow handlers for.
pub const SUBSCRIBED_EVENTS: [&str; 4] = [
    "pull_request",
    "check_suite",
    "pull_request_review",
    "issue_comment",
];

/// GitHub's registration probe. Recorded like anything else and acted on by
/// nothing, so pressing **Redeliver ping** is a working end-to-end setup test.
pub const PING_EVENT: &str = "ping";

/// What a delivery's row says happened to it.
pub const STATUS_RECEIVED: &str = "received";
pub const STATUS_IGNORED: &str = "ignored";
pub const STATUS_ERROR: &str = "error";

/// A generated secret: 40 alphanumeric characters, the same shape and source
/// every other token in this tree uses.
pub fn generate_secret() -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(40)
        .map(char::from)
        .collect()
}

/// Where GitHub delivers for this workspace.
///
/// Under `/api/` deliberately: `deploy/docker/nginx.conf.template` proxies only
/// `^/(api|mcp|healthz|\.well-known)/?` to the control plane, so a `/hooks/…`
/// path would be answered with the SPA's `index.html` and every delivery would
/// read as a green 200 that recorded nothing.
pub fn delivery_url(public_base_url: &str, workspace: WorkspaceId) -> String {
    format!(
        "{}/api/v1/hooks/github/{}",
        public_base_url.trim_end_matches('/'),
        workspace.0
    )
}

/// The value GitHub puts in `X-Hub-Signature-256` for this body and secret.
pub fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key length");
    mac.update(body);
    let hex: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256={hex}")
}

/// Does `header` sign `body` with `secret`?
///
/// Constant time in the comparison, via `Mac::verify_slice` — a byte-by-byte
/// `==` on a signature leaks, through timing, how many leading bytes a guess
/// got right, which turns forging one into a few thousand requests. The hex
/// decode ahead of it is on attacker-supplied text and reveals nothing about
/// the secret.
pub fn verify(secret: &str, body: &[u8], header: &str) -> bool {
    let Some(hex) = header.strip_prefix("sha256=") else {
        return false;
    };
    let Some(expected) = decode_hex(hex) else {
        return false;
    };
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key length");
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Does this delivery name the repository the workspace is a checkout of?
///
/// Case-insensitive, because the two sides case the name differently and always
/// have: `git_remote_normalized` rides `discovery::normalize_remote`, which
/// lowercases, while GitHub reports its canonical casing — so a repo genuinely
/// named `Acme/API` would fail every correct delivery under an exact compare
/// (the hazard `services/jobs.rs` documents for PR URLs).
///
/// **Nothing to compare is not a mismatch.** A workspace with no normalized
/// remote — a local bare repo, an empty project — has no repository for the
/// delivery to disagree with, and refusing it would make such a workspace
/// unable to receive at all. The signature is the gate; this is a consistency
/// assert on top of it, which is also why the repository is not the routing
/// key: `workspaces_remote_idx` is unique per TENANT, so two tenants may hold
/// the same repo and `full_name` identifies no single workspace.
pub fn repo_matches(git_remote_normalized: Option<&str>, repo_full_name: &str) -> bool {
    let Some(normalized) = git_remote_normalized.filter(|n| !n.trim().is_empty()) else {
        return true;
    };
    if repo_full_name.trim().is_empty() {
        return true;
    }
    let expected = normalized.to_lowercase();
    let got = repo_full_name.trim().to_lowercase();
    // `github.com/owner/repo` against `owner/repo`.
    expected == got || expected.strip_prefix("github.com/") == Some(got.as_str())
}

/// The status a well-formed, correctly-signed delivery is recorded at.
///
/// `ping` is GitHub's own registration probe rather than a repository fact —
/// there is nothing in it for a child card to ever act on, so it is filed as
/// deliberately ignored. Everything else is recorded as `received`: NO event is
/// acted upon by this card (NG-1), and `received` is what says "this arrived
/// and nothing has consumed it yet" to the children that will.
pub fn status_for(event: &str) -> &'static str {
    if event == PING_EVENT {
        STATUS_IGNORED
    } else {
        STATUS_RECEIVED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A vector from GitHub's own documentation of the scheme, so this is
    /// checked against GitHub rather than against itself.
    #[test]
    fn matches_githubs_published_vector() {
        assert_eq!(
            sign("It's a Secret to Everybody", b"Hello, World!"),
            "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17"
        );
    }

    #[test]
    fn a_tampered_body_or_a_wrong_secret_does_not_verify() {
        let sig = sign("s3cret", b"{\"a\":1}");
        assert!(verify("s3cret", b"{\"a\":1}", &sig));
        assert!(!verify("s3cret", b"{\"a\":2}", &sig));
        assert!(!verify("other", b"{\"a\":1}", &sig));
    }

    /// The framings that are not this scheme, and the malformed ones. Notably
    /// `notify::sign`'s own output: the two must never accept each other.
    #[test]
    fn a_header_in_any_other_shape_is_refused() {
        for header in [
            "",
            "sha1=aabb",
            "t=1,v1=aabb",
            "757107ea",
            "sha256=",
            "sha256=zz",
            "sha256=abc",
        ] {
            assert!(!verify("s3cret", b"{}", header), "{header}");
        }
    }

    #[test]
    fn the_repository_assert_folds_case_and_tolerates_nothing_to_compare() {
        assert!(repo_matches(Some("github.com/acme/api"), "acme/api"));
        assert!(repo_matches(Some("github.com/acme/api"), "Acme/API"));
        assert!(!repo_matches(Some("github.com/acme/api"), "acme/other"));
        // A local bare repo, and an organisation ping with no repository.
        assert!(repo_matches(None, "acme/api"));
        assert!(repo_matches(Some("github.com/acme/api"), ""));
    }

    #[test]
    fn the_delivery_url_lives_under_api_and_tolerates_a_trailing_slash() {
        let ws = WorkspaceId::new();
        let want = format!("https://nook.example/api/v1/hooks/github/{}", ws.0);
        assert_eq!(delivery_url("https://nook.example", ws), want);
        assert_eq!(delivery_url("https://nook.example/", ws), want);
    }
}
