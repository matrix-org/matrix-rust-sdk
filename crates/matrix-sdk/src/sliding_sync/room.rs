use std::{
    fmt::Debug,
    ops::Not,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use futures_signals::{signal::Mutable, signal_vec::MutableVec};
use matrix_sdk_base::deserialized_responses::SyncTimelineEvent;
use ruma::{
    api::client::sync::sync_events::{v4, UnreadNotificationsCount},
    events::AnySyncStateEvent,
    serde::Raw,
    OwnedRoomId,
};
use serde::{Deserialize, Serialize};
use tracing::{error, instrument};

use crate::{
    room::timeline::{EventTimelineItem, Timeline, TimelineBuilder},
    Client,
};

type AliveRoomTimeline = Arc<MutableVec<SyncTimelineEvent>>;

/// Room info as giving by the SlidingSync Feature.
#[derive(Debug, Clone)]
pub struct SlidingSyncRoom {
    client: Client,
    room_id: OwnedRoomId,
    inner: v4::SlidingSyncRoom,
    is_loading_more: Mutable<bool>,
    is_cold: Arc<AtomicBool>,
    prev_batch: Mutable<Option<String>>,
    timeline_queue: AliveRoomTimeline,
}

#[derive(Serialize, Deserialize)]
pub(super) struct FrozenSlidingSyncRoom {
    room_id: OwnedRoomId,
    inner: v4::SlidingSyncRoom,
    prev_batch: Option<String>,
    #[serde(rename = "timeline")]
    timeline_queue: Vec<SyncTimelineEvent>,
}

#[cfg(test)]
mod tests {
    use matrix_sdk_base::deserialized_responses::TimelineEvent;
    use ruma::{events::room::message::RoomMessageEventContent, RoomId};
    use serde_json::json;

    use super::*;

    #[test]
    fn test_frozen_sliding_sync_room_serialize() {
        let frozen_sliding_sync_room = FrozenSlidingSyncRoom {
            room_id: <&RoomId>::try_from("!29fhd83h92h0:example.com").unwrap().to_owned(),
            inner: v4::SlidingSyncRoom::default(),
            prev_batch: Some("let it go!".to_owned()),
            timeline_queue: vec![TimelineEvent {
                event: Raw::new(&json! ({
                    "content": RoomMessageEventContent::text_plain("let it gooo!"),
                    "type": "m.room.message",
                    "event_id": "$xxxxx:example.org",
                    "room_id": "!someroom:example.com",
                    "origin_server_ts": 2189,
                    "sender": "@bob:example.com",
                }))
                .unwrap()
                .cast(),
                encryption_info: None,
            }
            .into()],
        };

        assert_eq!(
            serde_json::to_string(&frozen_sliding_sync_room).unwrap(),
            "{\"room_id\":\"!29fhd83h92h0:example.com\",\"inner\":{},\"prev_batch\":\"let it go!\",\"timeline\":[{\"event\":{\"content\":{\"body\":\"let it gooo!\",\"msgtype\":\"m.text\"},\"event_id\":\"$xxxxx:example.org\",\"origin_server_ts\":2189,\"room_id\":\"!someroom:example.com\",\"sender\":\"@bob:example.com\",\"type\":\"m.room.message\"},\"encryption_info\":null}]}",
        );
    }
}

impl From<&SlidingSyncRoom> for FrozenSlidingSyncRoom {
    fn from(value: &SlidingSyncRoom) -> Self {
        let locked_tl = value.timeline_queue.lock_ref();
        let tl_len = locked_tl.len();
        // To not overflow the database, we only freeze the newest 10 items. On doing
        // so, we must drop the `prev_batch` key however, as we'd otherwise
        // create a gap between what we have loaded and where the
        // prev_batch-key will start loading when paginating backwards.
        let (prev_batch, timeline) = if tl_len > 10 {
            let pos = tl_len - 10;
            (None, locked_tl.iter().skip(pos).cloned().collect())
        } else {
            (value.prev_batch.lock_ref().clone(), locked_tl.to_vec())
        };
        FrozenSlidingSyncRoom {
            prev_batch,
            timeline_queue: timeline,
            room_id: value.room_id.clone(),
            inner: value.inner.clone(),
        }
    }
}

impl SlidingSyncRoom {
    pub(super) fn from_frozen(val: FrozenSlidingSyncRoom, client: Client) -> Self {
        let FrozenSlidingSyncRoom { room_id, inner, prev_batch, timeline_queue: timeline } = val;
        SlidingSyncRoom {
            client,
            room_id,
            inner,
            is_loading_more: Mutable::new(false),
            is_cold: Arc::new(AtomicBool::new(true)),
            prev_batch: Mutable::new(prev_batch),
            timeline_queue: Arc::new(MutableVec::new_with_values(timeline)),
        }
    }
}

impl SlidingSyncRoom {
    pub(crate) fn from(
        client: Client,
        room_id: OwnedRoomId,
        mut inner: v4::SlidingSyncRoom,
        timeline: Vec<SyncTimelineEvent>,
    ) -> Self {
        // we overwrite to only keep one copy
        inner.timeline = vec![];
        Self {
            client,
            room_id,
            is_loading_more: Mutable::new(false),
            is_cold: Arc::new(AtomicBool::new(false)),
            prev_batch: Mutable::new(inner.prev_batch.clone()),
            timeline_queue: Arc::new(MutableVec::new_with_values(timeline)),
            inner,
        }
    }

    /// RoomId of this SlidingSyncRoom
    pub fn room_id(&self) -> &OwnedRoomId {
        &self.room_id
    }

    /// Are we currently fetching more timeline events in this room?
    pub fn is_loading_more(&self) -> bool {
        *self.is_loading_more.lock_ref()
    }

    /// the `prev_batch` key to fetch more timeline events for this room
    pub fn prev_batch(&self) -> Option<String> {
        self.prev_batch.lock_ref().clone()
    }

    /// `Timeline` of this room
    pub async fn timeline(&self) -> Option<Timeline> {
        Some(self.timeline_builder()?.track_fully_read().build().await)
    }

    fn timeline_builder(&self) -> Option<TimelineBuilder> {
        if let Some(room) = self.client.get_room(&self.room_id) {
            let timeline_queue = self.timeline_queue.lock_ref().to_vec();
            let prev_batch = self.prev_batch.lock_ref().clone();
            Some(Timeline::builder(&room).events(prev_batch, timeline_queue))
        } else if let Some(invited_room) = self.client.get_invited_room(&self.room_id) {
            Some(Timeline::builder(&invited_room).events(None, vec![]))
        } else {
            error!(
                room_id = ?self.room_id,
                "Room not found in client. Can't provide a timeline for it"
            );
            None
        }
    }

    /// The latest timeline item of this room.
    ///
    /// Use `Timeline::latest_event` instead if you already have a timeline for
    /// this `SlidingSyncRoom`.
    #[instrument(skip_all, parent = &self.client.root_span)]
    pub async fn latest_event(&self) -> Option<EventTimelineItem> {
        self.timeline_builder()?.build().await.latest_event().await
    }

    /// This rooms name as calculated by the server, if any
    pub fn name(&self) -> Option<&str> {
        self.inner.name.as_deref()
    }

    /// Is this a direct message?
    pub fn is_dm(&self) -> Option<bool> {
        self.inner.is_dm
    }

    /// Was this an initial response.
    pub fn is_initial_response(&self) -> Option<bool> {
        self.inner.initial
    }

    /// Is there any unread notifications?
    pub fn has_unread_notifications(&self) -> bool {
        self.inner.unread_notifications.is_empty().not()
    }

    /// Get unread notifications.
    pub fn unread_notifications(&self) -> &UnreadNotificationsCount {
        &self.inner.unread_notifications
    }

    /// Get the required state.
    pub fn required_state(&self) -> &Vec<Raw<AnySyncStateEvent>> {
        &self.inner.required_state
    }

    pub(super) fn update(
        &mut self,
        room_data: &v4::SlidingSyncRoom,
        timeline_updates: Vec<SyncTimelineEvent>,
    ) {
        let v4::SlidingSyncRoom {
            name,
            initial,
            limited,
            is_dm,
            invite_state,
            unread_notifications,
            required_state,
            prev_batch,
            ..
        } = room_data;

        self.inner.unread_notifications = unread_notifications.clone();

        if name.is_some() {
            self.inner.name = name.clone();
        }
        if initial.is_some() {
            self.inner.initial = *initial;
        }
        if is_dm.is_some() {
            self.inner.is_dm = *is_dm;
        }
        if !invite_state.is_empty() {
            self.inner.invite_state = invite_state.clone();
        }
        if !required_state.is_empty() {
            self.inner.required_state = required_state.clone();
        }

        if let Some(batch) = prev_batch {
            self.prev_batch.lock_mut().replace(batch.clone());
        }

        // There is timeline updates.
        if !timeline_updates.is_empty() {
            if self.is_cold.load(Ordering::SeqCst) {
                // If we come from a cold storage, we overwrite the timeline queue with the
                // timeline updates.

                self.timeline_queue.lock_mut().replace_cloned(timeline_updates);
                self.is_cold.store(false, Ordering::SeqCst);
            } else if *limited {
                // The server alerted us that we missed items in between.

                self.timeline_queue.lock_mut().replace_cloned(timeline_updates);
            } else {
                // It's the hot path. We have new updates that must be added to the existing
                // timeline queue.

                let mut timeline_queue = self.timeline_queue.lock_mut();

                // If the `timeline_queue` contains:
                //     [D, E, F]
                // and if the `timeline_updates` contains:
                //     [A, B, C, D, E, F]
                // the resulting `timeline_queue` must be:
                //     [A, B, C, D, E, F]
                //
                // To do that, we find the longest suffix between `timeline_queue` and
                // `timeline_updates`, in this case:
                //     [D, E, F]
                // Remove the suffix from `timeline_updates`, we get `[A, B, C]` that is
                // prepended to `timeline_queue`.
                //
                // If the `timeline_queue` contains:
                //     [A, B, C, D, E, F]
                // and if the `timeline_updates` contains:
                //     [D, E, F]
                // the resulting `timeline_queue` must be:
                //     [A, B, C, D, E, F]
                //
                // To do that, we continue with the longest suffix. In this case, it is:
                //     [D, E, F]
                // Remove the suffix from `timeline_updates`, we get `[]`. It's empty, we don't
                // touch at `timeline_queue`.

                {
                    let timeline_queue_len = timeline_queue.len();
                    let timeline_updates_len = timeline_updates.len();

                    let position = match timeline_queue
                        .iter()
                        .rev()
                        .zip(timeline_updates.iter().rev())
                        .position(|(queue, update)| queue.event_id() != update.event_id())
                    {
                        // We have found a suffix that equals the size of `timeline_queue` or
                        // `timeline_update`, typically:
                        //     timeline_queue = [D, E, F]
                        //     timeline_update = [A, B, C, D, E, F]
                        // or
                        //     timeline_queue = [A, B, C, D, E, F]
                        //     timeline_update = [D, E, F]
                        // in both case, `position` will return `None` because we are looking for
                        // (from the end) an item that is different.
                        None => std::cmp::min(timeline_queue_len, timeline_updates_len),

                        // We may have found a suffix.
                        //
                        // If we have `Some(0)`, it means we don't have found a suffix. That's the
                        // hot path, `timeline_updates` will just be appended to `timeline_queue`.
                        //
                        // If we have `Some(n)` with `n > 0`, it means we have a prefix but it
                        // doesn't cover all `timeline_queue` or `timeline_update`, typically:
                        //     timeline_queue = [B, D, E, F]
                        //     timeline_update = [A, B, C, D, E, F]
                        // in this case, `position` will return `Some(3)`.
                        // That's annoying because it means we have an invalid `timeline_queue` or
                        // `timeline_update`, but let's try to do our best.
                        Some(position) => position,
                    };

                    if position == 0 {
                        // No prefix found.

                        timeline_queue.extend(timeline_updates);
                    } else {
                        // Prefix found.

                        let new_timeline_updates =
                            &timeline_updates[..timeline_updates_len - position];

                        if !new_timeline_updates.is_empty() {
                            for (at, update) in new_timeline_updates.iter().cloned().enumerate() {
                                timeline_queue.insert_cloned(at, update);
                            }
                        }
                    }
                }
            }
        } else if *limited {
            // The timeline updates are empty. But `limited` is set to true. It's a way to
            // alert that we are stale. In this case, we should just clear the
            // existing timeline.

            self.timeline_queue.lock_mut().clear();
        }
    }
}
