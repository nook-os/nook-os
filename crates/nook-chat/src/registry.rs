//! Per-instance live fan-out (MAIN-49 AC-3; per-user stream MAIN-117 AC-4/AC-6).
//!
//! Every event this instance publishes goes onto ONE broadcast firehose. Each
//! per-user websocket taps it with [`Registry::subscribe_all`] and forwards an
//! event only if the caller is authorized for that event's channel *at that
//! moment* (`channels::access`), so the delivery boundary is re-evaluated per
//! event — a membership change (a DM opened, a person added to an org) takes
//! effect live, with no reconnect. Cross-instance delivery rides the bus
//! (`bus.rs`), which republishes each remote event through [`publish_local`];
//! this is the local half, mirroring the control plane's registry/bus split.

use nook_types::ChatServerMessage;
use tokio::sync::broadcast;
use uuid::Uuid;

/// How many messages a slow subscriber may fall behind before it is lagged and
/// told to catch up rather than blocking the sender.
const CHANNEL_CAP: usize = 256;

/// The channel a server-message event belongs to. Both variants carry a
/// [`ChatMessage`], so a new post and an update (edit/delete/reaction — MAIN-116)
/// route the same way. `pub(crate)` so the per-user stream (MAIN-117) can
/// re-authorize each delivered event by its channel.
pub(crate) fn event_channel(event: &ChatServerMessage) -> Uuid {
    match event {
        ChatServerMessage::Message(m) | ChatServerMessage::MessageUpdated(m) => m.channel_id,
    }
}

pub struct Registry {
    /// This instance's id, so the bus can skip its own NOTIFYs and never deliver
    /// a message twice on the instance that posted it.
    instance: Uuid,
    /// Every event this instance publishes, in one stream. The per-user socket
    /// (MAIN-117) taps this and filters by membership per event, so a channel or
    /// DM gained mid-session delivers live without a reconnect — a fixed
    /// subscribe set resolved at connect could not (AC-6 "a user added begins
    /// receiving it").
    firehose: broadcast::Sender<ChatServerMessage>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            instance: Uuid::now_v7(),
            firehose: broadcast::channel(CHANNEL_CAP).0,
        }
    }

    pub fn instance(&self) -> Uuid {
        self.instance
    }

    /// Subscribe to EVERY event on this instance (MAIN-117): the per-user stream
    /// re-authorizes each by channel before forwarding, so this carries no
    /// authorization on its own — it is a superset the socket filters.
    pub fn subscribe_all(&self) -> broadcast::Receiver<ChatServerMessage> {
        self.firehose.subscribe()
    }

    /// Deliver an event to this instance's per-user streams via the firehose.
    /// Sending with no receivers is a no-op.
    pub fn publish_local(&self, event: ChatServerMessage) {
        let _ = self.firehose.send(event);
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use nook_types::ChatMessage;

    fn msg(channel: Uuid) -> ChatMessage {
        ChatMessage {
            id: Uuid::now_v7(),
            channel_id: channel,
            author_id: Uuid::now_v7(),
            author_name: None,
            body: "hi".into(),
            parent_message_id: None,
            reply_count: 0,
            last_reply_at: None,
            created_at: Utc::now(),
            reactions: Vec::new(),
            edited_at: None,
            deleted: false,
        }
    }

    /// The firehose carries EVERY published event, tagged by its channel, to
    /// every tap. Isolation is not done here — the per-user socket re-authorizes
    /// each event by `event_channel` before forwarding (AC-6). This proves the
    /// superset-plus-tag contract that filtering depends on.
    #[tokio::test]
    async fn the_firehose_carries_every_event_tagged_by_channel() {
        let reg = Registry::new();
        let channel_a = Uuid::now_v7();
        let channel_b = Uuid::now_v7();
        let mut rx = reg.subscribe_all();

        let on_a = msg(channel_a);
        let on_b = msg(channel_b);
        reg.publish_local(ChatServerMessage::Message(on_a.clone()));
        reg.publish_local(ChatServerMessage::Message(on_b.clone()));

        // Both events arrive on the single tap, each identifiable by channel so
        // the socket can authorize (and keep or drop) it.
        let first = rx.try_recv().expect("first event delivered");
        assert_eq!(event_channel(&first), channel_a);
        let second = rx.try_recv().expect("second event delivered");
        assert_eq!(event_channel(&second), channel_b);
        assert!(rx.try_recv().is_err(), "nothing else was delivered");
    }

    #[test]
    fn each_instance_has_a_distinct_id() {
        assert_ne!(Registry::new().instance(), Registry::new().instance());
    }
}
