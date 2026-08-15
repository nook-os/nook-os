//! The rules a report has to satisfy before it is stored (MAIN-603).
//!
//! Everything here is about the *envelope* — the key, the title, the size, the
//! count. Nothing here looks at `body_md`, and nothing ever should: Nook does
//! not parse report content (NG-1), so the only questions this file may ask
//! about a body are how long it is.
//!
//! The limits are here rather than inline in the route because they are the
//! part a caller has to be told about, and a refusal that does not name its own
//! rule sends an automation author to read source.

use crate::error::{ApiError, ApiResult};

/// Longest a report key may be. Long enough for `nightly-benchmark-summary`,
/// short enough that a key is still something a person reads at a glance.
pub const MAX_KEY_CHARS: usize = 64;

/// Longest a title may be (AC-7's spirit). Without it the body cap is a hole
/// you can drive through — 64 KiB refused in `body_md` and then written into
/// `title`, which is the field the sidebar renders as a card header.
pub const MAX_TITLE_CHARS: usize = 200;

/// AC-7: a body is at most 64 KiB. Bytes rather than characters, because the
/// thing being bounded is what the database and the wire have to carry.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// AC-7: a card carries at most twenty reports. A runaway producer must not
/// turn a card into a log file — and the way it would is a new key per run,
/// which the per-key upsert cannot catch.
pub const MAX_REPORTS_PER_TASK: i64 = 20;

/// The slug rule, in the words the refusal uses. One string so the check and
/// the message cannot drift.
const KEY_RULE: &str = "a report key is 1–64 characters of lowercase letters, digits and '-'";

/// AC-3. A key is an address, so it is validated the way a path segment has to
/// be: no case (two keys differing only in case would be two reports of the
/// same thing), no separators, nothing that needs escaping.
pub fn validate_key(key: &str) -> ApiResult<()> {
    if key.is_empty() || key.chars().count() > MAX_KEY_CHARS {
        return Err(ApiError::BadRequest(format!("{KEY_RULE} — {key:?} is not")));
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(ApiError::BadRequest(format!("{KEY_RULE} — {key:?} is not")));
    }
    Ok(())
}

/// The title, trimmed, or the reason it cannot be stored.
pub fn validate_title(title: &str) -> ApiResult<String> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return Err(ApiError::BadRequest(
            "a report needs a title — it is the heading the card renders it under".into(),
        ));
    }
    if trimmed.chars().count() > MAX_TITLE_CHARS {
        return Err(ApiError::BadRequest(format!(
            "a report title is at most {MAX_TITLE_CHARS} characters (this one is {})",
            trimmed.chars().count()
        )));
    }
    Ok(trimmed.to_string())
}

/// AC-7's body limit, named in its own refusal.
pub fn validate_body(body_md: &str) -> ApiResult<()> {
    if body_md.len() > MAX_BODY_BYTES {
        return Err(ApiError::BadRequest(format!(
            "a report body is at most {} (this one is {})",
            nook_types::human_size(MAX_BODY_BYTES as i64),
            nook_types::human_size(body_md.len() as i64)
        )));
    }
    Ok(())
}

/// AC-7's per-card cap, checked against the count of keys that already exist.
///
/// `existing_key` is what keeps a re-run working on a full card: replacing a
/// report does not add one, and refusing that would mean the twentieth
/// producer could never update its own output again.
pub fn check_room(count: i64, existing_key: bool) -> ApiResult<()> {
    if existing_key || count < MAX_REPORTS_PER_TASK {
        return Ok(());
    }
    Err(ApiError::BadRequest(format!(
        "this card already has the maximum of {MAX_REPORTS_PER_TASK} reports — \
         delete one, or write to a key that already exists"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    fn status(err: ApiError) -> StatusCode {
        axum::response::IntoResponse::into_response(err).status()
    }

    #[test]
    fn a_key_is_a_slug_and_a_refusal_names_the_rule() {
        for good in [
            "build",
            "loop-review",
            "ci-2",
            "x",
            &"a".repeat(MAX_KEY_CHARS),
        ] {
            validate_key(good).unwrap_or_else(|e| panic!("{good:?}: {e}"));
        }
        for bad in [
            "",
            "Build",
            "build report",
            "build_report",
            "build/report",
            "bui.ld",
            "ünicode",
            &"a".repeat(MAX_KEY_CHARS + 1),
        ] {
            let err = validate_key(bad).expect_err(bad);
            assert_eq!(status(clone_msg(&err)), StatusCode::BAD_REQUEST, "{bad:?}");
            assert!(
                err.to_string()
                    .contains("lowercase letters, digits and '-'"),
                "the refusal names the rule: {err}"
            );
        }
    }

    #[test]
    fn the_body_limit_is_bytes_and_the_refusal_says_both_numbers() {
        validate_body(&"a".repeat(MAX_BODY_BYTES)).expect("exactly at the limit is fine");
        let err = validate_body(&"a".repeat(MAX_BODY_BYTES + 1)).expect_err("one over");
        let msg = err.to_string();
        assert!(msg.contains("64.0 KB"), "names the limit: {msg}");
        assert!(msg.contains("at most"), "names the rule: {msg}");
    }

    #[test]
    fn a_full_card_still_lets_an_existing_key_be_replaced() {
        check_room(MAX_REPORTS_PER_TASK - 1, false).expect("room for one more");
        check_room(MAX_REPORTS_PER_TASK, true).expect("replacing is not adding");
        let err = check_room(MAX_REPORTS_PER_TASK, false).expect_err("full");
        assert!(
            err.to_string().contains(&MAX_REPORTS_PER_TASK.to_string()),
            "names the limit: {err}"
        );
    }

    #[test]
    fn a_title_is_required_and_bounded() {
        assert_eq!(validate_title("  Build  ").expect("trimmed"), "Build");
        validate_title("   ").expect_err("a heading with nothing in it");
        validate_title(&"t".repeat(MAX_TITLE_CHARS)).expect("at the limit");
        validate_title(&"t".repeat(MAX_TITLE_CHARS + 1)).expect_err("one over");
    }

    /// `ApiError` is not `Clone`, and asserting on both the status and the text
    /// needs it twice.
    fn clone_msg(err: &ApiError) -> ApiError {
        ApiError::BadRequest(err.to_string())
    }
}
