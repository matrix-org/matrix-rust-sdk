// Copyright 2025 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Sticky events ([MSC4354]), received over sliding sync through the
//! [MSC4480] extension.
//!
//! A sticky event is a message-like event that the server keeps delivering to
//! clients for a bounded duration (at most one hour), regardless of the
//! timeline limit. Clients fold the sticky events of a room into a map keyed by
//! `(sender, type, content.sticky_key)`, in which the event that is last to
//! expire wins, and from which entries disappear once they expire. MatrixRTC
//! uses this to track who is in a call.
//!
//! The map of a room is reachable through [`Room::sticky_events`]. It is fed
//! from the sync response, expires its entries in the background, and
//! broadcasts every visible change to its subscribers. Encrypted sticky events
//! that cannot be decrypted yet are kept aside and retried when their room key
//! arrives. Nothing is persisted: the server re-sends every sticky event that
//! is still live when the sliding sync connection starts over.
//!
//! [MSC4354]: https://github.com/matrix-org/matrix-spec-proposals/pull/4354
//! [MSC4480]: https://github.com/matrix-org/matrix-spec-proposals/pull/4480
//! [`Room::sticky_events`]: crate::Room::sticky_events

#[cfg(feature = "e2e-encryption")]
mod decrypt;
mod extract;
mod map;
mod task;

#[cfg(feature = "e2e-encryption")]
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use matrix_sdk_common::{
    deserialized_responses::{EncryptionInfo, TimelineEventKind},
    executor::AbortOnDrop,
};
use ruma::{
    MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId,
    events::{AnySyncTimelineEvent, TimelineEventType},
    serde::Raw,
};
use tokio::sync::{Notify, broadcast};
use tracing::{debug, warn};

#[cfg(feature = "e2e-encryption")]
pub(crate) use self::decrypt::{Decryption, decrypt, spawn_redecryptor};
use self::map::EphemeralMap;
pub(crate) use self::{
    extract::{Payload, StickyMeta, classify, resolve},
    map::Candidate,
};

/// The maximum number of encrypted sticky events kept aside per room while
/// waiting for their room keys. Beyond that, the oldest (which are the closest
/// to expiring) make way for new ones.
const MAX_PENDING: usize = 100;

/// The capacity of the broadcast channel behind [`StickyEvents::subscribe`].
const UPDATES_CHANNEL_CAPACITY: usize = 16;

/// The key of a sticky event in the map of a room.
///
/// MSC4354 keys the map by `(room_id, sender, type, content.sticky_key)`; the
/// room is implied by the map this key belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StickyKey {
    /// The sender of the event.
    pub sender: OwnedUserId,
    /// The type of the event, e.g. `m.rtc.member`.
    pub event_type: TimelineEventType,
    /// The `content.sticky_key` of the event.
    pub sticky_key: String,
}

/// A sticky event that is currently live in the map of a room.
#[derive(Clone, Debug)]
pub struct StickyEvent {
    /// The key of the event in the map.
    pub key: StickyKey,
    /// The event ID.
    pub event_id: OwnedEventId,
    /// The event, decrypted if it was encrypted, along with its encryption
    /// info.
    pub kind: TimelineEventKind,
    /// When the event stops being sticky.
    pub expires_at: MilliSecondsSinceUnixEpoch,
}

impl StickyEvent {
    /// The event, decrypted if it was encrypted.
    pub fn raw(&self) -> &Raw<AnySyncTimelineEvent> {
        self.kind.raw()
    }

    /// The encryption info of the event, if it was encrypted.
    pub fn encryption_info(&self) -> Option<&Arc<EncryptionInfo>> {
        self.kind.encryption_info()
    }
}

/// Why a sticky event disappeared from the map of a room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemovalReason {
    /// The event stopped being sticky.
    Expired,
    /// A newer event with the same key and an empty content (a removal, in
    /// MSC4354 terms) replaced it.
    Replaced,
    /// We left the room.
    RoomLeft,
}

/// A batch of visible changes to the map of a room.
///
/// One update is broadcast per processed sync response, and one per expiry
/// pass, so that subscribers see the net effect of each at once.
#[derive(Clone, Debug, Default)]
pub struct StickyEventsUpdate {
    /// The events that appeared under a key that had no live event.
    pub added: Vec<StickyEvent>,
    /// The events that replaced the live event of their key.
    pub updated: Vec<StickyEvent>,
    /// The keys whose live event disappeared, and why.
    pub removed: Vec<(StickyKey, RemovalReason)>,
}

impl StickyEventsUpdate {
    /// Whether this update carries no change at all.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.updated.is_empty() && self.removed.is_empty()
    }
}

/// An encrypted sticky event that couldn't be decrypted yet.
///
/// Without encryption support nothing ever reads the event back: it is kept
/// until it expires, like any other, for the sake of a single code path.
#[derive(Debug)]
pub(crate) struct PendingEvent {
    /// The sticky metadata of the event, with a [`Payload::Encrypted`].
    pub meta: StickyMeta,
    /// The encrypted event.
    #[cfg_attr(not(feature = "e2e-encryption"), allow(dead_code))]
    pub event: Raw<AnySyncTimelineEvent>,
}

impl PendingEvent {
    /// Return the Megolm session the event was encrypted with (if any).
    fn session_id(&self) -> Option<&str> {
        match &self.meta.payload {
            Payload::Encrypted { session_id } => session_id.as_deref(),
            Payload::Plain { .. } => None,
        }
    }
}

/// The sticky events of a room.
///
/// This is a cheap-to-clone handle over the map of a room, which every clone
/// (and every clone of the [`Room`](crate::Room) it belongs to) shares.
#[derive(Clone, Debug)]
pub struct StickyEvents {
    inner: Arc<StickyEventsInner>,
}

#[derive(Debug)]
struct StickyEventsInner {
    /// The ID of the room these sticky events belong to.
    room_id: OwnedRoomId,
    /// The mutable state of the room's sticky events.
    state: Mutex<State>,
    /// Broadcasts the visible changes of the map to subscribers.
    updates: broadcast::Sender<StickyEventsUpdate>,
    /// Notified whenever the state changed in a way that may require the
    /// maintenance task to wake up earlier than planned.
    changed: Arc<Notify>,
    /// The background task expiring entries and pending events. Spawned the
    /// first time there is something to expire, so that a room without sticky
    /// events costs nothing, and aborted when the last handle is dropped.
    task: OnceLock<AbortOnDrop<()>>,
}

/// The mutable state of a room's sticky events.
#[derive(Debug, Default)]
struct State {
    /// The map of live (and tombstoned) sticky events.
    map: EphemeralMap,
    /// Encrypted sticky events awaiting their room key, oldest first.
    pending: Vec<PendingEvent>,
}

impl StickyEvents {
    pub(crate) fn new(room_id: OwnedRoomId) -> Self {
        let (updates, _) = broadcast::channel(UPDATES_CHANNEL_CAPACITY);

        Self {
            inner: Arc::new(StickyEventsInner {
                room_id,
                state: Default::default(),
                updates,
                changed: Default::default(),
                task: OnceLock::new(),
            }),
        }
    }

    /// The room these sticky events belong to.
    pub fn room_id(&self) -> &RoomId {
        &self.inner.room_id
    }

    /// The sticky events that are currently live, in no particular order.
    pub fn live(&self) -> Vec<StickyEvent> {
        self.inner.state.lock().unwrap().map.live(now_ms()).collect()
    }

    /// Subscribe to the changes of the map.
    ///
    /// A subscriber that falls behind receives a
    /// [`Lagged`](broadcast::error::RecvError::Lagged) error, after which it
    /// should reconcile with [`live`](Self::live).
    pub fn subscribe(&self) -> broadcast::Receiver<StickyEventsUpdate> {
        self.inner.updates.subscribe()
    }

    /// Apply resolved sticky events received at `now` to the map.
    pub(crate) fn ingest(&self, now: u64, candidates: Vec<Candidate>) {
        if candidates.is_empty() {
            return;
        }

        self.ensure_task();

        let update = self.inner.state.lock().unwrap().map.apply(now, candidates);
        self.inner.changed.notify_one();
        self.publish(update);
    }

    /// Keep encrypted sticky events aside until their room key arrives.
    pub(crate) fn park(&self, pending: Vec<PendingEvent>) {
        if pending.is_empty() {
            return;
        }

        self.ensure_task();

        {
            let mut state = self.inner.state.lock().unwrap();

            for event in pending {
                if event.session_id().is_none() {
                    debug!(
                        room_id = %self.inner.room_id,
                        event_id = %event.meta.event_id,
                        "Dropping an encrypted sticky event without a session ID"
                    );
                    continue;
                }

                state.pending.push(event);
            }

            if state.pending.len() > MAX_PENDING {
                warn!(
                    room_id = %self.inner.room_id,
                    "Too many encrypted sticky events await their room key, dropping the oldest"
                );
                let excess = state.pending.len() - MAX_PENDING;
                state.pending.drain(..excess);
            }
        }

        self.inner.changed.notify_one();
    }

    /// Take the pending encrypted sticky events that were encrypted with one of
    /// `session_ids`, or all of them if `None`.
    #[cfg(feature = "e2e-encryption")]
    pub(crate) fn take_pending(&self, session_ids: Option<&BTreeSet<String>>) -> Vec<PendingEvent> {
        let mut state = self.inner.state.lock().unwrap();

        match session_ids {
            None => std::mem::take(&mut state.pending),
            Some(session_ids) => {
                let (taken, kept) =
                    std::mem::take(&mut state.pending).into_iter().partition(|event| {
                        event.session_id().is_some_and(|id| session_ids.contains(id))
                    });
                state.pending = kept;
                taken
            }
        }
    }

    /// Whether any encrypted sticky event awaits its room key.
    #[cfg(feature = "e2e-encryption")]
    pub(crate) fn has_pending(&self) -> bool {
        !self.inner.state.lock().unwrap().pending.is_empty()
    }

    /// Forget every sticky event, e.g. because we left the room.
    pub(crate) fn clear(&self) {
        let removed = {
            let mut state = self.inner.state.lock().unwrap();
            state.pending.clear();
            state.map.clear(now_ms())
        };

        self.publish(StickyEventsUpdate {
            removed: removed.into_iter().map(|key| (key, RemovalReason::RoomLeft)).collect(),
            ..Default::default()
        });
    }

    fn publish(&self, update: StickyEventsUpdate) {
        if !update.is_empty() {
            // Failing to send only means there is no subscriber.
            let _ = self.inner.updates.send(update);
        }
    }

    fn ensure_task(&self) {
        self.inner
            .task
            .get_or_init(|| task::spawn(Arc::downgrade(&self.inner), self.inner.changed.clone()));
    }
}

impl StickyEventsInner {
    /// Drop what has expired at `now`, broadcast the visible removals, and
    /// return when the next entry is due to expire, if any.
    fn expire(&self, now: u64) -> Option<u64> {
        let (removed, next) = {
            let mut state = self.state.lock().unwrap();

            let removed = state.map.evict_expired(now);
            state.pending.retain(|event| event.meta.expires_at > now);

            let next_pending = state.pending.iter().map(|event| event.meta.expires_at).min();
            let next = match (state.map.next_expiry(), next_pending) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };

            (removed, next)
        };

        if !removed.is_empty() {
            let _ = self.updates.send(StickyEventsUpdate {
                removed: removed.into_iter().map(|key| (key, RemovalReason::Expired)).collect(),
                ..Default::default()
            });
        }

        next
    }
}

/// The current time, in milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> u64 {
    MilliSecondsSinceUnixEpoch::now().get().into()
}

/// A weak handle to the state of a room's sticky events, for background
/// tasks.
type WeakStickyEvents = Weak<StickyEventsInner>;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use assert_matches2::assert_matches;
    use matrix_sdk_common::deserialized_responses::TimelineEventKind;
    use matrix_sdk_test::async_test;
    use ruma::{
        events::{AnySyncTimelineEvent, TimelineEventType},
        owned_event_id, owned_user_id, room_id,
        serde::Raw,
    };
    use serde_json::json;
    use tokio::sync::broadcast::error::TryRecvError;

    use super::{
        Candidate, PendingEvent, RemovalReason, StickyEvents, StickyKey, StickyMeta, now_ms,
    };
    use crate::sticky::Payload;

    fn raw_event() -> Raw<AnySyncTimelineEvent> {
        serde_json::from_value(json!({
            "type": "m.rtc.member",
            "sender": "@alice:localhost",
            "event_id": "$a:localhost",
            "origin_server_ts": 1,
            "content": { "msc4354_sticky_key": "slot" },
        }))
        .unwrap()
    }

    fn candidate(sticky_key: &str, expires_at: u64) -> Candidate {
        Candidate {
            key: StickyKey {
                sender: owned_user_id!("@alice:localhost"),
                event_type: TimelineEventType::from("m.rtc.member"),
                sticky_key: sticky_key.to_owned(),
            },
            event_id: owned_event_id!("$a:localhost"),
            order_ts: expires_at,
            expires_at,
            is_tombstone: false,
            kind: TimelineEventKind::PlainText { event: raw_event() },
        }
    }

    fn pending(session_id: Option<&str>, expires_at: u64) -> PendingEvent {
        PendingEvent {
            meta: StickyMeta {
                sender: owned_user_id!("@alice:localhost"),
                event_id: owned_event_id!("$a:localhost"),
                order_ts: expires_at,
                expires_at,
                payload: Payload::Encrypted { session_id: session_id.map(ToOwned::to_owned) },
            },
            event: raw_event(),
        }
    }

    #[async_test]
    async fn test_ingest_broadcasts_and_exposes_live_events() {
        let sticky = StickyEvents::new(room_id!("!room:localhost").to_owned());
        let mut subscriber = sticky.subscribe();

        let now = now_ms();
        sticky.ingest(now, vec![candidate("slot", now + 60_000)]);

        let live = sticky.live();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].key.sticky_key, "slot");
        assert!(live[0].encryption_info().is_none());

        let update = subscriber.try_recv().unwrap();
        assert_eq!(update.added.len(), 1);
        assert_eq!(update.added[0].key.sticky_key, "slot");
    }

    #[async_test]
    async fn test_expired_events_are_evicted_in_the_background() {
        let sticky = StickyEvents::new(room_id!("!room:localhost").to_owned());
        let mut subscriber = sticky.subscribe();

        let now = now_ms();
        sticky.ingest(now, vec![candidate("slot", now + 50)]);
        assert_matches!(subscriber.try_recv(), Ok(update));
        assert_eq!(update.added.len(), 1);

        // No further action on our side: the maintenance task must notice the
        // expiry and tell us.
        let update = tokio::time::timeout(Duration::from_secs(5), subscriber.recv())
            .await
            .expect("the expiry should be broadcast in time")
            .unwrap();
        assert_matches!(update.removed.as_slice(), [(key, RemovalReason::Expired)]);
        assert_eq!(key.sticky_key, "slot");

        assert!(sticky.live().is_empty());
    }

    #[async_test]
    async fn test_clear_broadcasts_removals() {
        let sticky = StickyEvents::new(room_id!("!room:localhost").to_owned());
        let mut subscriber = sticky.subscribe();

        let now = now_ms();
        sticky.ingest(now, vec![candidate("slot", now + 60_000)]);
        sticky.park(vec![pending(Some("session"), now + 60_000)]);
        let _ = subscriber.try_recv().unwrap();

        sticky.clear();

        let update = subscriber.try_recv().unwrap();
        assert_matches!(update.removed.as_slice(), [(key, RemovalReason::RoomLeft)]);
        assert_eq!(key.sticky_key, "slot");
        assert!(sticky.live().is_empty());
        assert_matches!(subscriber.try_recv(), Err(TryRecvError::Empty));
    }

    #[cfg(feature = "e2e-encryption")]
    #[async_test]
    async fn test_pending_events_are_taken_by_session_id() {
        use std::collections::BTreeSet;

        let sticky = StickyEvents::new(room_id!("!room:localhost").to_owned());

        let now = now_ms();
        sticky.park(vec![
            pending(Some("session1"), now + 60_000),
            pending(Some("session2"), now + 60_000),
            // Nothing could ever decrypt this one: it isn't kept.
            pending(None, now + 60_000),
        ]);

        let taken = sticky.take_pending(Some(&BTreeSet::from(["session2".to_owned()])));
        assert_eq!(taken.len(), 1);
        assert_matches!(&taken[0].meta.payload, Payload::Encrypted { session_id });
        assert_eq!(session_id.as_deref(), Some("session2"));

        assert!(sticky.has_pending());
        let taken = sticky.take_pending(None);
        assert_eq!(taken.len(), 1);
        assert!(!sticky.has_pending());
    }

    #[cfg(feature = "e2e-encryption")]
    #[async_test]
    async fn test_pending_events_are_capped() {
        use std::collections::BTreeSet;

        use super::MAX_PENDING;

        let sticky = StickyEvents::new(room_id!("!room:localhost").to_owned());

        let now = now_ms();
        // One more than the cap, the oldest first.
        let mut events = vec![pending(Some("oldest"), now + 60_000)];
        events.extend((0..MAX_PENDING).map(|_| pending(Some("newer"), now + 60_000)));
        sticky.park(events);

        // The oldest made way.
        assert!(sticky.take_pending(Some(&BTreeSet::from(["oldest".to_owned()]))).is_empty());
        assert_eq!(sticky.take_pending(None).len(), MAX_PENDING);
    }

    #[cfg(feature = "e2e-encryption")]
    #[async_test]
    async fn test_expired_pending_events_are_pruned_in_the_background() {
        let sticky = StickyEvents::new(room_id!("!room:localhost").to_owned());

        sticky.park(vec![pending(Some("session"), now_ms() + 50)]);
        assert!(sticky.has_pending());

        tokio::time::timeout(Duration::from_secs(5), async {
            while sticky.has_pending() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the pending event should be pruned in time");
    }
}
