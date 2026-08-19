//! `@slug` in a card's description, and what it resolves to (MAIN-632).
//!
//! A card carries one `workspace_id` and a run is placed in that one repo. A
//! feature spanning a frontend and a backend had no way to say so, so the spec
//! was written blind and the builder guessed at the other side's contract.
//! Naming the other repo in the body is how it says so.
//!
//! The parse is here and pure; resolution and storage are the task repository's
//! ([`crate::repo::tasks::TaskRepository::sync_workspace_refs`]), because they
//! belong in the same transaction as the description write that caused them.

/// The workspace slugs a description names with `@`, in order, deduplicated.
///
/// Deliberately generous about what it CAPTURES and strict about nothing:
/// resolution is what decides whether a token is a reference, and an
/// unresolvable one is left as plain text rather than rejected (AC-2). Writing
/// a description is not a place to fail on a typo.
///
/// The one thing it will not do is read an email address as a mention: `@` has
/// to open a word, so `dev@nookos.local` yields nothing while `@nookos` yields
/// `nookos`.
pub fn mentioned_slugs(description: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let bytes: Vec<char> = description.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != '@' {
            i += 1;
            continue;
        }
        // `@` must OPEN a word. Without this every email address in a body is a
        // mention of its own domain, and a card that quotes one reads as
        // referencing a workspace nobody named.
        let opens_a_word = i == 0 || !bytes[i - 1].is_alphanumeric();
        let mut j = i + 1;
        while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || matches!(bytes[j], '-' | '_'))
        {
            j += 1;
        }
        if opens_a_word && j > i + 1 {
            // Lowercased because a slug is, and trimmed of the separators a
            // sentence leaves behind — `@web-` at the end of a clause is the
            // same reference as `@web`.
            let token: String = bytes[i + 1..j]
                .iter()
                .collect::<String>()
                .to_lowercase()
                .trim_matches(|c| c == '-' || c == '_')
                .to_string();
            if !token.is_empty() && !out.contains(&token) {
                out.push(token);
            }
        }
        i = j.max(i + 1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::mentioned_slugs;

    #[test]
    fn two_references_are_both_found_in_order() {
        assert_eq!(
            mentioned_slugs("Wire @nook-web against @nook-api's new endpoint."),
            vec!["nook-web", "nook-api"]
        );
    }

    #[test]
    fn an_email_address_is_not_a_mention() {
        assert_eq!(
            mentioned_slugs("Ask dev@nookos.local about it."),
            Vec::<String>::new()
        );
    }

    /// The same workspace named twice is one reference: the join table's
    /// primary key says so, and the parser must not make the insert rely on it.
    #[test]
    fn the_same_slug_twice_is_one_reference() {
        assert_eq!(mentioned_slugs("@web and @web again"), vec!["web"]);
    }

    #[test]
    fn punctuation_ends_a_slug_and_case_is_folded() {
        assert_eq!(
            mentioned_slugs("see @Nook-Web, then stop"),
            vec!["nook-web"]
        );
        assert_eq!(mentioned_slugs("(@web)"), vec!["web"]);
        assert_eq!(mentioned_slugs("@web."), vec!["web"]);
    }

    /// A bare `@` is not a reference and must not become an empty slug — an
    /// empty lookup would match nothing, but it would also be a query per `@`
    /// in every code fence a description quotes.
    #[test]
    fn a_bare_at_is_nothing() {
        assert_eq!(mentioned_slugs("@ @@ a @ b"), Vec::<String>::new());
    }
}
