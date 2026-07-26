//! Canonical client-IP resolution behind a reverse proxy.
//!
//! The per-IP rate limiter is only as trustworthy as the address it keys on,
//! and `X-Forwarded-For` is a request header — anyone can write anything into
//! it. So we believe it ONLY when the immediate peer is a proxy we configured
//! to trust (`NOOK_TRUSTED_PROXIES`); otherwise the peer socket IP is the
//! client, full stop. That single rule is what stops an attacker from spoofing
//! their source to dodge (or frame) the limiter.
//!
//! Kept as a pure function so the whole decision table is unit-testable without
//! a server or a socket.

use ipnet::IpNet;
use std::net::IpAddr;

/// Resolve the client IP from the peer socket address, the `X-Forwarded-For`
/// header, and the configured trusted-proxy CIDRs.
///
/// - Untrusted peer → the peer IP; XFF is ignored entirely (it is attacker
///   controlled at that point).
/// - Trusted peer → walk the XFF chain RIGHT-TO-LEFT (nearest hop first),
///   discarding addresses that are themselves trusted proxies, and take the
///   first untrusted address as the client.
/// - Fall back to the peer IP for a direct connection, an absent/empty header,
///   an all-trusted chain, or a malformed header/hop. A malformed hop is a
///   forged or broken chain, so we do NOT reach past it to an unconditional
///   leftmost value.
pub fn resolve_client_ip(peer: IpAddr, xff: Option<&str>, trusted: &[IpNet]) -> IpAddr {
    let is_trusted = |ip: IpAddr| trusted.iter().any(|net| net.contains(&ip));

    // The peer is who we are actually talking to. Unless it is a proxy we chose
    // to trust, nothing it forwarded can be believed.
    if !is_trusted(peer) {
        return peer;
    }

    let Some(chain) = xff.map(str::trim).filter(|s| !s.is_empty()) else {
        return peer;
    };

    // Right-to-left: the rightmost entry is what our trusted proxy observed,
    // the leftmost is the most easily forged. Skip trusted hops; the first
    // untrusted address is the real client.
    for hop in chain.split(',').rev() {
        match hop.trim().parse::<IpAddr>() {
            Ok(ip) if is_trusted(ip) => continue,
            Ok(ip) => return ip,
            // A malformed hop means the chain cannot be trusted past this point;
            // fall back to the peer rather than guessing.
            Err(_) => return peer,
        }
    }

    // Every hop was a trusted proxy (or the header was only trusted proxies) —
    // there is no untrusted client address to name, so use the peer.
    peer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(s: &str) -> IpNet {
        s.parse().unwrap()
    }
    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// A direct connection from an untrusted peer: XFF is present but ignored,
    /// because a client that is not a trusted proxy can put anything there.
    #[test]
    fn direct_connection_ignores_xff() {
        let trusted = [net("10.0.0.0/8")];
        assert_eq!(
            resolve_client_ip(ip("203.0.113.9"), Some("1.2.3.4"), &trusted),
            ip("203.0.113.9")
        );
    }

    /// A spoofed XFF from an UNTRUSTED peer must never be honored — this is the
    /// attack the whole resolver exists to defeat.
    #[test]
    fn spoofed_xff_from_untrusted_peer_is_ignored() {
        let trusted = [net("10.0.0.0/8")];
        // Attacker claims to be a trusted internal address AND a victim.
        assert_eq!(
            resolve_client_ip(ip("203.0.113.9"), Some("10.0.0.1, 198.51.100.7"), &trusted),
            ip("203.0.113.9"),
            "an untrusted peer's XFF is worthless"
        );
    }

    /// One trusted proxy in front: the single XFF entry is the client.
    #[test]
    fn trusted_single_proxy() {
        let trusted = [net("10.0.0.0/8")];
        assert_eq!(
            resolve_client_ip(ip("10.0.0.1"), Some("198.51.100.7"), &trusted),
            ip("198.51.100.7")
        );
    }

    /// A chain of trusted proxies: skip every trusted hop right-to-left and
    /// return the first untrusted address (the real client).
    #[test]
    fn trusted_multi_proxy_chain() {
        let trusted = [net("10.0.0.0/8")];
        // client -> edge(10.0.0.9) -> app(peer 10.0.0.1)
        assert_eq!(
            resolve_client_ip(ip("10.0.0.1"), Some("198.51.100.7, 10.0.0.9"), &trusted),
            ip("198.51.100.7")
        );
    }

    /// An all-trusted chain has no client address to name → the peer.
    #[test]
    fn all_trusted_chain_falls_back_to_peer() {
        let trusted = [net("10.0.0.0/8")];
        assert_eq!(
            resolve_client_ip(ip("10.0.0.1"), Some("10.0.0.9, 10.0.0.5"), &trusted),
            ip("10.0.0.1")
        );
    }

    /// A malformed hop stops the walk at the peer rather than reaching past it.
    #[test]
    fn malformed_hop_falls_back_to_peer() {
        let trusted = [net("10.0.0.0/8")];
        assert_eq!(
            resolve_client_ip(ip("10.0.0.1"), Some("not-an-ip, 10.0.0.9"), &trusted),
            ip("10.0.0.1")
        );
        // A completely malformed header, too.
        assert_eq!(
            resolve_client_ip(ip("10.0.0.1"), Some("garbage"), &trusted),
            ip("10.0.0.1")
        );
    }

    /// An empty or absent header on a trusted peer → the peer.
    #[test]
    fn empty_or_absent_header_uses_peer() {
        let trusted = [net("10.0.0.0/8")];
        assert_eq!(
            resolve_client_ip(ip("10.0.0.1"), None, &trusted),
            ip("10.0.0.1")
        );
        assert_eq!(
            resolve_client_ip(ip("10.0.0.1"), Some("   "), &trusted),
            ip("10.0.0.1")
        );
    }

    /// No trusted proxies configured (the default): always the peer, whatever
    /// the header says.
    #[test]
    fn no_trusted_proxies_always_uses_peer() {
        assert_eq!(
            resolve_client_ip(ip("10.0.0.1"), Some("198.51.100.7"), &[]),
            ip("10.0.0.1")
        );
    }

    /// IPv6 works the same way: a trusted v6 proxy forwards a v6 client.
    #[test]
    fn ipv6_trusted_proxy() {
        let trusted = [net("2001:db8::/32")];
        assert_eq!(
            resolve_client_ip(ip("2001:db8::1"), Some("2001:db8:abcd::42"), &trusted),
            // The forwarded client is inside the trusted /32 here, so it is a
            // hop; with no untrusted address the peer stands.
            ip("2001:db8::1")
        );
        // A v6 proxy forwarding a public v6 client returns that client.
        assert_eq!(
            resolve_client_ip(ip("2001:db8::1"), Some("2607:f8b0::1"), &trusted),
            ip("2607:f8b0::1")
        );
    }

    /// A mixed trust list covering both families resolves each correctly.
    #[test]
    fn ipv4_and_ipv6_mixed_trust() {
        let trusted = [net("10.0.0.0/8"), net("2001:db8::/32")];
        // v4 peer, v4 client.
        assert_eq!(
            resolve_client_ip(ip("10.0.0.1"), Some("203.0.113.5"), &trusted),
            ip("203.0.113.5")
        );
        // v6 peer, v6 client.
        assert_eq!(
            resolve_client_ip(ip("2001:db8::9"), Some("2607:f8b0::99"), &trusted),
            ip("2607:f8b0::99")
        );
    }
}
