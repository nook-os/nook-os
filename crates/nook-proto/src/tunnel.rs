//! What a tunnel is CALLED, as a pure function (MAIN-402 AC-5).
//!
//! A tunnel is reached at `<label>.<zone>`, and the label is derived from the
//! workspace and node names rather than typed by a human — a name somebody has
//! to invent is a name that collides, gets typo'd, and ends up in a URL nobody
//! can guess a second time.
//!
//! Derivation lives here, beside the frames, because both the control plane and
//! the CLI need the same answer and neither should own it. It is deliberately
//! pure: no DNS, no database, no clock. The collision rule takes the set of
//! labels already in use and hands back one that is not among them, so the
//! caller decides what "already in use" means (this tenant's, this zone's) and
//! this file never has to know.
//!
//! Nothing here makes a tunnel REACHABLE — that is 9b's HTTP surface (NG-1).

/// The longest a single DNS label may be, and the reason the suffix in
/// [`unique_subdomain`] eats into the stem rather than being appended to it.
pub const MAX_LABEL: usize = 63;

/// Turn arbitrary text into one DNS label: lowercase, `[a-z0-9-]`, no leading
/// or trailing hyphen, never empty.
///
/// `_` and `.` become `-` rather than being dropped, because dropping them
/// silently merges words — `api_v2` and `apiv2` are different repositories and
/// must not become the same host.
pub fn slug_label(input: &str) -> String {
    let out = clean(input);
    if out.is_empty() {
        return "tunnel".into();
    }
    out
}

/// The cleaning half, WITHOUT the fallback — it may return empty.
///
/// Separate because the fallback is right for a whole label and wrong for a
/// component of one: `subdomain_for("api", "")` must be `api`, not
/// `api-tunnel`. A missing node name should contribute nothing, and the only
/// way to know it contributed nothing is to see the empty string.
fn clean(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_dash = true; // leading dashes are not allowed
    for ch in input.chars() {
        let c = ch.to_ascii_lowercase();
        let mapped = if c.is_ascii_alphanumeric() {
            Some(c)
        } else if matches!(c, '-' | '_' | '.' | '/' | ' ') {
            Some('-')
        } else {
            // Anything else — punctuation, an emoji, a non-ASCII letter — is
            // dropped rather than transliterated. A wrong transliteration is a
            // wrong hostname, and there is no right one for every script.
            None
        };
        match mapped {
            Some('-') if last_dash => {}
            Some(c) => {
                out.push(c);
                last_dash = c == '-';
            }
            None => {}
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out.truncate(MAX_LABEL);
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// The label a workspace/node pair wants, before collisions are considered.
///
/// Workspace first, because that is what a person is looking for; the node
/// disambiguates one repo checked out on four machines, which is the shape
/// MAIN-323 already found people have.
pub fn subdomain_for(workspace: &str, node: &str) -> String {
    let w = clean(workspace);
    let n = clean(node);
    if n.is_empty() || w == n {
        return slug_label(&w);
    }
    slug_label(&format!("{w}-{n}"))
}

/// The first label not already taken: the stem, then `-2`, `-3`, …
///
/// Numeric rather than random: a person reading `api-azul-2` can tell it is the
/// second of something, where a hash suffix tells them nothing and cannot be
/// guessed back. The suffix EATS INTO the stem when the label would otherwise
/// exceed [`MAX_LABEL`] — appending would produce a name DNS rejects, which is
/// a failure at resolve time rather than here.
pub fn unique_subdomain(stem: &str, taken: &dyn Fn(&str) -> bool) -> String {
    let stem = slug_label(stem);
    if !taken(&stem) {
        return stem;
    }
    for n in 2..=9999u32 {
        let suffix = format!("-{n}");
        let room = MAX_LABEL.saturating_sub(suffix.len());
        let mut base = stem.clone();
        base.truncate(room);
        while base.ends_with('-') {
            base.pop();
        }
        let candidate = format!("{base}{suffix}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    // Ten thousand collisions on one stem is not a naming problem any more.
    // Returning the stem lets the caller's own uniqueness constraint refuse it
    // loudly, rather than this loop inventing something unpredictable.
    stem
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_is_dns_safe() {
        assert_eq!(slug_label("Acme/API Server"), "acme-api-server");
        assert_eq!(slug_label("api_v2"), "api-v2");
        assert_eq!(slug_label("--api--"), "api");
        assert_eq!(slug_label("UPPER"), "upper");
    }

    #[test]
    fn separators_never_merge_two_words() {
        // `api_v2` and `apiv2` are different repositories. A derivation that
        // dropped the separator would give them one hostname and silently send
        // traffic to whichever won.
        assert_ne!(slug_label("api_v2"), slug_label("apiv2"));
    }

    #[test]
    fn a_name_with_nothing_usable_still_yields_a_label() {
        // An empty host is not a degraded name, it is an unusable one.
        assert_eq!(slug_label("→→→"), "tunnel");
        assert_eq!(slug_label(""), "tunnel");
    }

    #[test]
    fn a_label_fits_dns_and_does_not_end_in_a_hyphen_after_truncation() {
        let long = format!("{}-x", "a".repeat(MAX_LABEL - 1));
        let out = slug_label(&long);
        assert!(out.len() <= MAX_LABEL, "{}", out.len());
        assert!(!out.ends_with('-'), "{out}");
    }

    #[test]
    fn the_node_disambiguates_one_repo_on_several_machines() {
        assert_eq!(subdomain_for("acme/api", "azul"), "acme-api-azul");
        assert_eq!(subdomain_for("acme/api", "crimson"), "acme-api-crimson");
    }

    #[test]
    fn a_node_that_adds_nothing_is_left_out() {
        assert_eq!(subdomain_for("api", "api"), "api");
        assert_eq!(subdomain_for("api", ""), "api");
    }

    #[test]
    fn collisions_suffix_in_a_readable_order() {
        let taken = |s: &str| matches!(s, "api" | "api-2" | "api-3");
        assert_eq!(unique_subdomain("api", &taken), "api-4");
        // Nothing taken → the stem itself, unsuffixed.
        assert_eq!(unique_subdomain("api", &|_: &str| false), "api");
    }

    #[test]
    fn a_suffix_eats_into_the_stem_rather_than_overflowing_dns() {
        // Appending would produce a label DNS rejects — a failure at resolve
        // time, far from here, and impossible to read back to this function.
        let stem = "a".repeat(MAX_LABEL);
        let out = unique_subdomain(&stem, &|s: &str| s == stem);
        assert!(out.len() <= MAX_LABEL, "{}", out.len());
        assert!(out.ends_with("-2"), "{out}");
    }

    #[test]
    fn the_taken_set_decides_uniqueness_not_this_module() {
        // The caller owns the scope of "already in use" — this tenant's, this
        // zone's — and passing a closure is what keeps that decision out of
        // here.
        let calls = std::cell::Cell::new(0);
        let out = unique_subdomain("api", &|_| {
            calls.set(calls.get() + 1);
            false
        });
        assert_eq!(out, "api");
        assert_eq!(calls.get(), 1, "asked exactly once when the stem is free");
    }
}
