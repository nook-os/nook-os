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

use std::collections::HashMap;
use std::sync::Mutex;

use nook_types::ChatServerMessage;
use tokio::sync::broadcast;
use uuid::Uuid;

/// How many messages a slow subscriber may fall behind before it is lagged and
/// told to catch up rather than blocking the sender.
const CHANNEL_CAP: usize = 256;

/// The channel a server-message event belongs to, or `None` for a
/// person-scoped event that no channel gate can answer for (MAIN-115: owner
/// ruling, option (a)). The match is EXHAUSTIVE on purpose — no default arm —
/// so every future variant consciously picks channel- or person-scoped
/// authorization rather than inheriting one silently.
pub(crate) fn event_channel(event: &ChatServerMessage) -> Option<Uuid> {
    match event {
        ChatServerMessage::Message(m) | ChatServerMessage::MessageUpdated(m) => Some(m.channel_id),
        // Typing belongs to its channel and inherits the existing per-event
        // gate unchanged (AC-2/AC-4).
        ChatServerMessage::Typing { channel_id, .. } => Some(*channel_id),
        // Presence is a person↔person relation; the socket routes it through
        // the person-scoped visibility check instead (AC-4).
        ChatServerMessage::Presence { .. } => None,
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
    /// How many live sockets each PERSON holds on this instance (MAIN-115
    /// AC-1/AC-5). Ephemeral by construction — zero schema; the map dies with
    /// the process, exactly as the connections it counts do. Presence edges
    /// (0→1, 1→0) are what get broadcast, so two tabs stay one "online".
    sockets: Mutex<HashMap<Uuid, usize>>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            instance: Uuid::now_v7(),
            firehose: broadcast::channel(CHANNEL_CAP).0,
            sockets: Mutex::new(HashMap::new()),
        }
    }

    /// Count a socket for `person`. True when it is their FIRST on this
    /// instance — the online edge worth announcing.
    pub fn socket_opened(&self, person: Uuid) -> bool {
        let mut sockets = self.sockets.lock().unwrap();
        let n = sockets.entry(person).or_insert(0);
        *n += 1;
        *n == 1
    }

    /// Un-count a socket for `person`. True when it was their LAST — the
    /// offline edge worth announcing.
    pub fn socket_closed(&self, person: Uuid) -> bool {
        let mut sockets = self.sockets.lock().unwrap();
        match sockets.get_mut(&person) {
            Some(n) if *n > 1 => {
                *n -= 1;
                false
            }
            Some(_) => {
                sockets.remove(&person);
                true
            }
            // A close with no open is a caller bug; never underflow into a
            // phantom "offline" storm.
            None => false,
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
        assert_eq!(event_channel(&first), Some(channel_a));
        let second = rx.try_recv().expect("second event delivered");
        assert_eq!(event_channel(&second), Some(channel_b));
        assert!(rx.try_recv().is_err(), "nothing else was delivered");
    }

    #[test]
    fn each_instance_has_a_distinct_id() {
        assert_ne!(Registry::new().instance(), Registry::new().instance());
    }

    /// AC-5's dedupe: only the FIRST socket announces online and only the LAST
    /// announces offline — two tabs are one presence, and closing one of two
    /// says nothing.
    #[test]
    fn presence_edges_fire_on_first_open_and_last_close_only() {
        let reg = Registry::new();
        let person = Uuid::now_v7();
        assert!(reg.socket_opened(person), "first socket: the online edge");
        assert!(!reg.socket_opened(person), "second tab: silent");
        assert!(
            !reg.socket_closed(person),
            "one of two closed: still online"
        );
        assert!(reg.socket_closed(person), "last closed: the offline edge");
        assert!(
            !reg.socket_closed(person),
            "a stray close never underflows into a phantom offline"
        );
    }

    /// A person-scoped event has no channel, and the match is exhaustive — a
    /// new variant must consciously pick a gate.
    #[test]
    fn presence_is_person_scoped_and_typing_is_channel_scoped() {
        let channel = Uuid::now_v7();
        assert_eq!(
            event_channel(&ChatServerMessage::Typing {
                channel_id: channel,
                person: Uuid::now_v7(),
                display_name: None,
            }),
            Some(channel)
        );
        assert_eq!(
            event_channel(&ChatServerMessage::Presence {
                person: Uuid::now_v7(),
                online: true,
            }),
            None
        );
    }
}
