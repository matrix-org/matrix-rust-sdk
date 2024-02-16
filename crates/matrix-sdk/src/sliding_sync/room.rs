use std::{
    fmt::Debug,
    sync::{Arc, RwLock},
};

use eyeball_im::Vector;
use matrix_sdk_base::deserialized_responses::SyncTimelineEvent;
use ruma::{api::client::sync::sync_events::v4, OwnedRoomId, RoomId};
use serde::{Deserialize, Serialize};

use crate::Client;

/// The state of a [`SlidingSyncRoom`].
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub enum SlidingSyncRoomState {
    /// The room is not loaded, i.e. not updates have been received yet.
    #[default]
    NotLoaded,

    /// The room has been preloaded, i.e. its values come from the cache, but no
    /// updates have been received yet.
    Preloaded,

    /// The room has received updates.
    Loaded,
}

/// A Sliding Sync Room.
///
/// It contains some information about a specific room, along with a queue of
/// events for the timeline.
///
/// It is OK to clone this type as much as you need: cloning it is cheap, and
/// shallow. All clones of the same value are sharing the same state.
#[derive(Debug, Clone)]
pub struct SlidingSyncRoom {
    inner: Arc<SlidingSyncRoomInner>,
}

impl SlidingSyncRoom {
    /// Create a new `SlidingSyncRoom`.
    pub fn new(client: Client, room_id: OwnedRoomId, timeline: Vec<SyncTimelineEvent>) -> Self {
        Self {
            inner: Arc::new(SlidingSyncRoomInner {
                client,
                room_id,
                state: RwLock::new(SlidingSyncRoomState::NotLoaded),
                timeline_queue: RwLock::new(timeline.into()),
            }),
        }
    }

    /// Get the room ID of this `SlidingSyncRoom`.
    pub fn room_id(&self) -> &RoomId {
        &self.inner.room_id
    }

    /// Get a copy of the cached timeline events.
    ///
    /// Note: This API only exists temporarily, it *will* be removed in the
    /// future.
    pub fn timeline_queue(&self) -> Vector<SyncTimelineEvent> {
        self.inner.timeline_queue.read().unwrap().clone()
    }

    /// Get a clone of the associated client.
    pub fn client(&self) -> Client {
        self.inner.client.clone()
    }

    pub(super) fn update(
        &mut self,
        room_data: v4::SlidingSyncRoom,
        timeline_updates: Vec<SyncTimelineEvent>,
    ) {
        let v4::SlidingSyncRoom { limited, .. } = room_data;

        let mut state = self.inner.state.write().unwrap();

        {
            let mut timeline_queue = self.inner.timeline_queue.write().unwrap();

            // There are timeline updates.
            if !timeline_updates.is_empty() {
                if let SlidingSyncRoomState::Preloaded = *state {
                    // If the room has been read from the cache, we overwrite the timeline queue
                    // with the timeline updates.

                    timeline_queue.clear();
                    timeline_queue.extend(timeline_updates);
                } else if limited {
                    // The server alerted us that we missed items in between.

                    timeline_queue.clear();
                    timeline_queue.extend(timeline_updates);
                } else {
                    // It's the hot path. We have new updates that must be added to the existing
                    // timeline queue.

                    timeline_queue.extend(timeline_updates);
                }
            } else if limited {
                // No timeline updates, but `limited` is set to true. It's a way to
                // alert that we are stale. In this case, we should just clear the
                // existing timeline.

                timeline_queue.clear();
            }
        }

        *state = SlidingSyncRoomState::Loaded;
    }

    pub(super) fn from_frozen(frozen_room: FrozenSlidingSyncRoom, client: Client) -> Self {
        let FrozenSlidingSyncRoom { room_id } = frozen_room;

        Self {
            inner: Arc::new(SlidingSyncRoomInner {
                client,
                room_id,
                state: RwLock::new(SlidingSyncRoomState::Preloaded),
                timeline_queue: RwLock::default(),
            }),
        }
    }
}

#[cfg(test)]
impl SlidingSyncRoom {
    fn state(&self) -> SlidingSyncRoomState {
        *self.inner.state.read().unwrap()
    }

    fn set_state(&mut self, state: SlidingSyncRoomState) {
        *self.inner.state.write().unwrap() = state;
    }
}

#[derive(Debug)]
struct SlidingSyncRoomInner {
    /// The client, used to fetch [`Room`][crate::Room].
    client: Client,

    /// The room ID.
    room_id: OwnedRoomId,

    /// Internal state of `Self`.
    state: RwLock<SlidingSyncRoomState>,

    /// A queue of received events, used to build a
    /// [`Timeline`][crate::Timeline].
    ///
    /// Given a room, its size is theoretically unbounded: we'll accumulate
    /// events in this list, until we reach a limited sync, in which case
    /// we'll clear it.
    timeline_queue: RwLock<Vector<SyncTimelineEvent>>,
}

/// A “frozen” [`SlidingSyncRoom`], i.e. that can be written into, or read from
/// a store.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct FrozenSlidingSyncRoom {
    pub(super) room_id: OwnedRoomId,
}

impl From<&SlidingSyncRoom> for FrozenSlidingSyncRoom {
    fn from(value: &SlidingSyncRoom) -> Self {
        Self { room_id: value.inner.room_id.clone() }
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_base::deserialized_responses::TimelineEvent;
    use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
    use matrix_sdk_test::async_test;
    use ruma::{
        api::client::sync::sync_events::v4, events::room::message::RoomMessageEventContent,
        room_id, serde::Raw, RoomId,
    };
    use serde_json::json;
    use wiremock::MockServer;

    use crate::{
        sliding_sync::{FrozenSlidingSyncRoom, SlidingSyncRoom, SlidingSyncRoomState},
        test_utils::logged_in_client,
    };

    macro_rules! room_response {
        ( $( $json:tt )+ ) => {
            serde_json::from_value::<v4::SlidingSyncRoom>(
                json!( $( $json )+ )
            ).unwrap()
        };
    }

    async fn new_room(room_id: &RoomId) -> SlidingSyncRoom {
        new_room_with_timeline(room_id, vec![]).await
    }

    async fn new_room_with_timeline(
        room_id: &RoomId,
        timeline: Vec<SyncTimelineEvent>,
    ) -> SlidingSyncRoom {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        SlidingSyncRoom::new(client, room_id.to_owned(), timeline)
    }

    #[async_test]
    async fn test_state_from_not_loaded() {
        let mut room = new_room(room_id!("!foo:bar.org")).await;

        assert_eq!(room.state(), SlidingSyncRoomState::NotLoaded);

        // Update with an empty response, but it doesn't matter.
        room.update(room_response!({}), vec![]);

        assert_eq!(room.state(), SlidingSyncRoomState::Loaded);
    }

    #[async_test]
    async fn test_state_from_preloaded() {
        let mut room = new_room(room_id!("!foo:bar.org")).await;

        room.set_state(SlidingSyncRoomState::Preloaded);

        // Update with an empty response, but it doesn't matter.
        room.update(room_response!({}), vec![]);

        assert_eq!(room.state(), SlidingSyncRoomState::Loaded);
    }

    #[async_test]
    async fn test_room_room_id() {
        let room_id = room_id!("!foo:bar.org");
        let room = new_room(room_id).await;

        assert_eq!(room.room_id(), room_id);
    }

    #[async_test]
    async fn test_timeline_queue_initially_empty() {
        let room = new_room(room_id!("!foo:bar.org")).await;

        assert!(room.timeline_queue().is_empty());
    }

    macro_rules! timeline_event {
        (from $sender:literal with id $event_id:literal at $ts:literal: $message:literal) => {
            TimelineEvent::new(
                Raw::new(&json!({
                    "content": RoomMessageEventContent::text_plain($message),
                    "type": "m.room.message",
                    "event_id": $event_id,
                    "room_id": "!foo:bar.org",
                    "origin_server_ts": $ts,
                    "sender": $sender,
                }))
                .unwrap()
                .cast()
            ).into()
        };
    }

    macro_rules! assert_timeline_queue_event_ids {
        (
            with $( $timeline_queue:ident ).* {
                $(
                    $nth:literal => $event_id:literal
                ),*
                $(,)*
            }
        ) => {
            let timeline = & $( $timeline_queue ).*;

            $(
                assert_eq!(timeline[ $nth ].event.deserialize().unwrap().event_id(), $event_id);
            )*
        };
    }

    #[async_test]
    async fn test_timeline_queue_initially_not_empty() {
        let room = new_room_with_timeline(
            room_id!("!foo:bar.org"),
            vec![
                timeline_event!(from "@alice:baz.org" with id "$x0:baz.org" at 0: "message 0"),
                timeline_event!(from "@alice:baz.org" with id "$x1:baz.org" at 1: "message 1"),
            ],
        )
        .await;

        {
            let timeline_queue = room.timeline_queue();

            assert_eq!(room.state(), SlidingSyncRoomState::NotLoaded);
            assert_eq!(timeline_queue.len(), 2);
            assert_timeline_queue_event_ids!(
                with timeline_queue {
                    0 => "$x0:baz.org",
                    1 => "$x1:baz.org",
                }
            );
        }
    }

    #[async_test]
    async fn test_timeline_queue_update_with_empty_timeline() {
        let mut room = new_room_with_timeline(
            room_id!("!foo:bar.org"),
            vec![
                timeline_event!(from "@alice:baz.org" with id "$x0:baz.org" at 0: "message 0"),
                timeline_event!(from "@alice:baz.org" with id "$x1:baz.org" at 1: "message 1"),
            ],
        )
        .await;

        {
            let timeline_queue = room.timeline_queue();

            assert_eq!(room.state(), SlidingSyncRoomState::NotLoaded);
            assert_eq!(timeline_queue.len(), 2);
            assert_timeline_queue_event_ids!(
                with timeline_queue {
                    0 => "$x0:baz.org",
                    1 => "$x1:baz.org",
                }
            );
        }

        room.update(room_response!({}), vec![]);

        // The queue is unmodified.
        {
            let timeline_queue = room.timeline_queue();

            assert_eq!(room.state(), SlidingSyncRoomState::Loaded);
            assert_eq!(timeline_queue.len(), 2);
            assert_timeline_queue_event_ids!(
                with timeline_queue {
                    0 => "$x0:baz.org",
                    1 => "$x1:baz.org",
                }
            );
        }
    }

    #[async_test]
    async fn test_timeline_queue_update_with_empty_timeline_and_with_limited() {
        let mut room = new_room_with_timeline(
            room_id!("!foo:bar.org"),
            vec![
                timeline_event!(from "@alice:baz.org" with id "$x0:baz.org" at 0: "message 0"),
                timeline_event!(from "@alice:baz.org" with id "$x1:baz.org" at 1: "message 1"),
            ],
        )
        .await;

        {
            let timeline_queue = room.timeline_queue();

            assert_eq!(room.state(), SlidingSyncRoomState::NotLoaded);
            assert_eq!(timeline_queue.len(), 2);
            assert_timeline_queue_event_ids!(
                with timeline_queue {
                    0 => "$x0:baz.org",
                    1 => "$x1:baz.org",
                }
            );
        }

        room.update(
            room_response!({
                "limited": true
            }),
            vec![],
        );

        // The queue has been emptied.
        {
            let timeline_queue = room.timeline_queue();

            assert_eq!(room.state(), SlidingSyncRoomState::Loaded);
            assert_eq!(timeline_queue.len(), 0);
        }
    }

    #[async_test]
    async fn test_timeline_queue_update_from_preloaded() {
        let mut room = new_room_with_timeline(
            room_id!("!foo:bar.org"),
            vec![
                timeline_event!(from "@alice:baz.org" with id "$x0:baz.org" at 0: "message 0"),
                timeline_event!(from "@alice:baz.org" with id "$x1:baz.org" at 1: "message 1"),
            ],
        )
        .await;

        room.set_state(SlidingSyncRoomState::Preloaded);

        {
            let timeline_queue = room.timeline_queue();

            assert_eq!(room.state(), SlidingSyncRoomState::Preloaded);
            assert_eq!(timeline_queue.len(), 2);
            assert_timeline_queue_event_ids!(
                with timeline_queue {
                    0 => "$x0:baz.org",
                    1 => "$x1:baz.org",
                }
            );
        }

        room.update(
            room_response!({}),
            vec![
                timeline_event!(from "@alice:baz.org" with id "$x2:baz.org" at 2: "message 2"),
                timeline_event!(from "@alice:baz.org" with id "$x3:baz.org" at 3: "message 3"),
            ],
        );

        // The queue is emptied, and new events are appended.
        {
            let timeline_queue = room.timeline_queue();

            assert_eq!(room.state(), SlidingSyncRoomState::Loaded);
            assert_eq!(timeline_queue.len(), 2);
            assert_timeline_queue_event_ids!(
                with timeline_queue {
                    0 => "$x2:baz.org",
                    1 => "$x3:baz.org",
                }
            );
        }
    }

    #[async_test]
    async fn test_timeline_queue_update_from_not_loaded() {
        let mut room = new_room_with_timeline(
            room_id!("!foo:bar.org"),
            vec![
                timeline_event!(from "@alice:baz.org" with id "$x0:baz.org" at 0: "message 0"),
                timeline_event!(from "@alice:baz.org" with id "$x1:baz.org" at 1: "message 1"),
            ],
        )
        .await;

        {
            let timeline_queue = room.timeline_queue();

            assert_eq!(room.state(), SlidingSyncRoomState::NotLoaded);
            assert_eq!(timeline_queue.len(), 2);
            assert_timeline_queue_event_ids!(
                with timeline_queue {
                    0 => "$x0:baz.org",
                    1 => "$x1:baz.org",
                }
            );
        }

        room.update(
            room_response!({}),
            vec![
                timeline_event!(from "@alice:baz.org" with id "$x2:baz.org" at 2: "message 2"),
                timeline_event!(from "@alice:baz.org" with id "$x3:baz.org" at 3: "message 3"),
            ],
        );

        // New events are appended to the queue.
        {
            let timeline_queue = room.timeline_queue();

            assert_eq!(room.state(), SlidingSyncRoomState::Loaded);
            assert_eq!(timeline_queue.len(), 4);
            assert_timeline_queue_event_ids!(
                with timeline_queue {
                    0 => "$x0:baz.org",
                    1 => "$x1:baz.org",
                    2 => "$x2:baz.org",
                    3 => "$x3:baz.org",
                }
            );
        }
    }

    #[async_test]
    async fn test_timeline_queue_update_from_not_loaded_with_limited() {
        let mut room = new_room_with_timeline(
            room_id!("!foo:bar.org"),
            vec![
                timeline_event!(from "@alice:baz.org" with id "$x0:baz.org" at 0: "message 0"),
                timeline_event!(from "@alice:baz.org" with id "$x1:baz.org" at 1: "message 1"),
            ],
        )
        .await;

        {
            let timeline_queue = room.timeline_queue();

            assert_eq!(room.state(), SlidingSyncRoomState::NotLoaded);
            assert_eq!(timeline_queue.len(), 2);
            assert_timeline_queue_event_ids!(
                with timeline_queue {
                    0 => "$x0:baz.org",
                    1 => "$x1:baz.org",
                }

            );
        }

        room.update(
            room_response!({
                "limited": true,
            }),
            vec![
                timeline_event!(from "@alice:baz.org" with id "$x2:baz.org" at 2: "message 2"),
                timeline_event!(from "@alice:baz.org" with id "$x3:baz.org" at 3: "message 3"),
            ],
        );

        // The queue is emptied, and new events are appended.
        {
            let timeline_queue = room.timeline_queue();

            assert_eq!(room.state(), SlidingSyncRoomState::Loaded);
            assert_eq!(timeline_queue.len(), 2);
            assert_timeline_queue_event_ids!(
                with timeline_queue {
                    0 => "$x2:baz.org",
                    1 => "$x3:baz.org",
                }
            );
        }
    }

    #[test]
    fn test_frozen_sliding_sync_room_serialization() {
        let frozen_room =
            FrozenSlidingSyncRoom { room_id: room_id!("!29fhd83h92h0:example.com").to_owned() };

        let serialized = serde_json::to_value(&frozen_room).unwrap();

        assert_eq!(
            serialized,
            json!({
                "room_id": "!29fhd83h92h0:example.com",
            })
        );

        let deserialized = serde_json::from_value::<FrozenSlidingSyncRoom>(serialized).unwrap();

        assert_eq!(deserialized.room_id, frozen_room.room_id);
    }
}
