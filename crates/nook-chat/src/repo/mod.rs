//! Chat data access behind intent-named traits (MAIN-257, the repository
//! chain's nook-chat leg).
//!
//! Three aggregates, three traits — categories fold into channels, because a
//! category has no meaning apart from the channels it groups:
//!
//! - [`channels::ChannelRepository`] — `chat_channels`,
//!   `chat_channel_categories`, `chat_channel_participants`,
//!   `chat_read_cursors`.
//! - [`messages::MessageRepository`] — `chat_messages`, `chat_reactions`,
//!   `chat_message_revisions`.
//! - [`dms::DmRepository`] — direct messages, which are channels with
//!   `owner_type = 'dm'` plus the person-directory reads that decide who may
//!   open one.
//!
//! **Cross-crate reads have nowhere else to go.** Chat reads `users` and
//! `tenants` — who a user is as a person, which org their tenant belongs
//! to, what role they hold. That is nook-control's identity data, and on every
//! previous card in this chain such a read was handed to its owning aggregate's
//! repository. Here it cannot be: `IdentityRepository` lives in another crate.
//! So those reads stay in nook-chat, named for the *question* they answer
//! (`person_in_org`, `org_of_tenant`, `tenant_role`, `may_dm`) rather than the
//! table they touch, so nothing here looks like a general-purpose users API.
//!
//! The WS registry and bus keep their own mechanism (AC-1); they hold no SQL.
//!
//! Methods are intent-named and coarse; no `sqlx` type appears in any
//! signature, and row mapping lives inside the impls (AC-2).

/// What a repository can fail with. Deliberately NOT `sqlx::Error`: a trait
/// signature that names the driver leaks the engine into every caller and into
/// every fake, which is exactly what this layer exists to prevent (AC-1).
///
/// Only one failure is worth distinguishing at a call site — a uniqueness
/// clash, which callers turn into a 409 with their own wording. Everything else
/// is a 500 whatever caused it, so it collapses into one variant rather than
/// re-exporting the driver's taxonomy.
#[derive(Debug)]
pub(crate) enum RepoError {
    /// A unique constraint rejected the write (a duplicate channel name).
    Conflict,
    Other,
}

pub(crate) type RepoResult<T> = Result<T, RepoError>;

impl From<nook_db::DbError> for RepoError {
    fn from(e: nook_db::DbError) -> Self {
        if e.is_unique_violation() {
            RepoError::Conflict
        } else {
            RepoError::Other
        }
    }
}

pub(crate) mod channels;
pub(crate) mod dms;
#[cfg(test)]
pub(crate) mod fakes;
pub(crate) mod messages;
