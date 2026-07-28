//! Per-channel live fan-out (MAIN-49 AC-3).
//!
//! Each subscribed websocket holds a `broadcast::Receiver`; a posted message is
//! published to its channel's sender and every current subscriber on THIS
//! instance receives it. Cross-instance delivery rides the bus (`bus.rs`); this
//! is the local half, mirroring the control plane's registry/bus split.

use dashmap::DashMap;
use nook_types::ChatServerMessage;
use tokio::sync::broadcast;
use uuid::Uuid;

/// How many messages a slow subscriber may fall behind before it is lagged and
/// told to catch up rather than blocking the sender.
const CHANNEL_CAP: usize = 256;

/// The channel a server-message event belongs to. Both variants carry a
/// [`ChatMessage`], so a new post and an update (edit/delete/reaction — MAIN-116)
/// route to the same per-channel subscribers.
fn event_channel(event: &ChatServerMessage) -> Uuid {
    match event {
        ChatServerMessage::Message(m) | ChatServerMessage::MessageUpdated(m) => m.channel_id,
    }
}

pub struct Registry {
    /// This instance's id, so the bus can skip its own NOTIFYs and never deliver
    /// a message twice on the instance that posted it.
    instance: Uuid,
    channels: DashMap<Uuid, broadcast::Sender<ChatServerMessage>>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            instance: Uuid::now_v7(),
            channels: DashMap::new(),
        }
    }

    pub fn instance(&self) -> Uuid {
        self.instance
    }

    /// Subscribe a websocket to a channel's live events (new messages AND
    /// updates — MAIN-116 AC-5).
    pub fn subscribe(&self, channel_id: Uuid) -> broadcast::Receiver<ChatServerMessage> {
        self.sender(channel_id).subscribe()
    }

    /// Deliver an event to this instance's subscribers of its channel. Sending
    /// with no receivers is a no-op (nobody is watching that channel here).
    pub fn publish_local(&self, event: ChatServerMessage) {
        if let Some(tx) = self.channels.get(&event_channel(&event)) {
            let _ = tx.send(event);
        }
    }

    fn sender(&self, channel_id: Uuid) -> broadcast::Sender<ChatServerMessage> {
        self.channels
            .entry(channel_id)
            .or_insert_with(|| broadcast::channel(CHANNEL_CAP).0)
            .clone()
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

    /// A subscriber only receives its own channel's events — the fan-out never
    /// crosses channels, and since a channel belongs to exactly one tenant this
    /// is the local half of tenant isolation (AC-3/AC-5).
    #[tokio::test]
    async fn a_subscriber_only_receives_its_own_channels_messages() {
        let reg = Registry::new();
        let channel_a = Uuid::now_v7();
        let channel_b = Uuid::now_v7();
        let mut rx = reg.subscribe(channel_a);

        // A message on another channel must not reach this subscriber.
        reg.publish_local(ChatServerMessage::Message(msg(channel_b)));
        // A message on the subscribed channel must.
        let sent = msg(channel_a);
        reg.publish_local(ChatServerMessage::Message(sent.clone()));

        let got = rx.try_recv().expect("the channel_a message is delivered");
        let ChatServerMessage::Message(got) = got else {
            panic!("expected a Message event");
        };
        assert_eq!(got.id, sent.id);
        assert!(rx.try_recv().is_err(), "nothing else was delivered");
    }

    #[test]
    fn each_instance_has_a_distinct_id() {
        assert_ne!(Registry::new().instance(), Registry::new().instance());
    }
}
