//! Re-export of the shared HTTP error (MAIN-274).
//!
//! `ApiError` moved to `nook-errors` so nook-chat lands on the same type rather
//! than a hand-mirrored copy. It is re-exported from its historic path
//! deliberately: every `use crate::error::{ApiError, ApiResult}` in this crate
//! — 73 files of them — keeps working untouched, so the move is provably a
//! relocation and not a rewrite.

pub use nook_errors::{ApiError, ApiResult};
