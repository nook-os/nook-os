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
//! **Cross-crate reads have nowhere else to go.** Chat reads `public.users` and
//! `public.tenants` — who a user is as a person, which org their tenant belongs
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

pub(crate) mod channels;
pub(crate) mod dms;
pub(crate) mod messages;
