use std::{
    fmt::Debug,
    ops::Not,
    sync::{Arc, RwLock},
};

use eyeball_im::Vector;
use matrix_sdk_base::deserialized_responses::SyncTimelineEvent;
use ruma::{
    api::client::sync::sync_events::{v4, UnreadNotificationsCount},
    events::AnySyncStateEvent,
    serde::Raw,
    OwnedRoomId, RoomId,
};
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
    pub fn new(
        client: Client,
        room_id: OwnedRoomId,
        inner: v4::SlidingSyncRoom,
        timeline: Vec<SyncTimelineEvent>,
    ) -> Self {
        Self {
            inner: Arc::new(SlidingSyncRoomInner {
                client,
                room_id,
                inner: RwLock::new(inner),
                state: RwLock::new(SlidingSyncRoomState::NotLoaded),
                timeline_queue: RwLock::new(timeline.into()),
            }),
        }
    }

    /// Get the room ID of this `SlidingSyncRoom`.
    pub fn room_id(&self) -> &RoomId {
        &self.inner.room_id
    }

    /// This rooms name as calculated by the server, if any
    pub fn name(&self) -> Option<String> {
        let inner = self.inner.inner.read().unwrap();

        inner.name.to_owned()
    }

    /// Is this a direct message?
    pub fn is_dm(&self) -> Option<bool> {
        let inner = self.inner.inner.read().unwrap();

        inner.is_dm
    }

    /// Was this an initial response?
    pub fn is_initial_response(&self) -> Option<bool> {
        let inner = self.inner.inner.read().unwrap();

        inner.initial
    }

    /// Is there any unread notifications?
    pub fn has_unread_notifications(&self) -> bool {
        let inner = self.inner.inner.read().unwrap();

        inner.unread_notifications.is_empty().not()
    }

    /// Get unread notifications.
    pub fn unread_notifications(&self) -> UnreadNotificationsCount {
        let inner = self.inner.inner.read().unwrap();

        inner.unread_notifications.clone()
    }

    /// Get the required state.
    pub fn required_state(&self) -> Vec<Raw<AnySyncStateEvent>> {
        self.inner.inner.read().unwrap().required_state.clone()
    }

    /// Get the token for back-pagination.
    pub fn prev_batch(&self) -> Option<String> {
        self.inner.inner.read().unwrap().prev_batch.clone()
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

    /// Find the latest event in this room
    pub fn latest_event(&self) -> Option<SyncTimelineEvent> {
        self.inner.client.get_room(&self.inner.room_id).and_then(|room| room.latest_event())
    }

    pub(super) fn update(
        &mut self,
        room_data: v4::SlidingSyncRoom,
        timeline_updates: Vec<SyncTimelineEvent>,
    ) {
        let v4::SlidingSyncRoom {
            name,
            initial,
            limited,
            is_dm,
            unread_notifications,
            required_state,
            prev_batch,
            ..
        } = room_data;

        {
            let mut inner = self.inner.inner.write().unwrap();

            inner.unread_notifications = unread_notifications;

            // The server might not send some parts of the response, because they were sent
            // before and the server wants to save bandwidth. So let's update the values
            // only when they exist.

            if name.is_some() {
                inner.name = name;
            }

            if initial.is_some() {
                inner.initial = initial;
            }

            if is_dm.is_some() {
                inner.is_dm = is_dm;
            }

            if !required_state.is_empty() {
                inner.required_state = required_state;
            }

            if prev_batch.is_some() {
                inner.prev_batch = prev_batch;
            }
        }

        let mut state = self.inner.state.write().unwrap();

        {
            let mut timeline_queue = self.inner.timeline_queue.write().unwrap();

            // There is timeline updates.
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
                // The timeline updates are empty. But `limited` is set to true. It's a way to
                // alert that we are stale. In this case, we should just clear the
                // existing timeline.

                timeline_queue.clear();
            }
        }

        *state = SlidingSyncRoomState::Loaded;
    }

    pub(super) fn from_frozen(frozen_room: FrozenSlidingSyncRoom, client: Client) -> Self {
        let FrozenSlidingSyncRoom { room_id, inner, timeline_queue } = frozen_room;

        Self {
            inner: Arc::new(SlidingSyncRoomInner {
                client,
                room_id,
                inner: RwLock::new(inner),
                state: RwLock::new(SlidingSyncRoomState::Preloaded),
                timeline_queue: RwLock::new(timeline_queue),
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
    /// The client, used to fetch [`room::Common`][crate::room::Common].
    client: Client,

    /// The room ID.
    room_id: OwnedRoomId,

    /// The room representation as a `SlidingSync`'s response from the server.
    ///
    /// We update this response when an update is needed.
    inner: RwLock<v4::SlidingSyncRoom>,

    /// Internal state of `Self`.
    state: RwLock<SlidingSyncRoomState>,

    /// A queue of received events, used to build a
    /// [`Timeline`][crate::Timeline].
    timeline_queue: RwLock<Vector<SyncTimelineEvent>>,
}

/// A “frozen” [`SlidingSyncRoom`], i.e. that can be written into, or read from
/// a store.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct FrozenSlidingSyncRoom {
    pub(super) room_id: OwnedRoomId,
    pub(super) inner: v4::SlidingSyncRoom,
    #[serde(rename = "timeline")]
    pub(super) timeline_queue: Vector<SyncTimelineEvent>,
}

/// Number of timeline events to keep when [`SlidingSyncRoom`] is saved in the
/// cache.
const NUMBER_OF_TIMELINE_EVENTS_TO_KEEP_FOR_THE_CACHE: usize = 10;

impl From<&SlidingSyncRoom> for FrozenSlidingSyncRoom {
    fn from(value: &SlidingSyncRoom) -> Self {
        let timeline_queue = &value.inner.timeline_queue.read().unwrap();
        let timeline_length = timeline_queue.len();

        let mut inner = value.inner.inner.read().unwrap().clone();

        // To not overflow the cache, we only freeze the newest N items. On doing
        // so, we must drop the `prev_batch` key however, as we'd otherwise
        // create a gap between what we have loaded and where the
        // prev_batch-key will start loading when paginating backwards.
        let timeline_queue = if timeline_length > NUMBER_OF_TIMELINE_EVENTS_TO_KEEP_FOR_THE_CACHE {
            inner.prev_batch = None;

            (*timeline_queue)
                .iter()
                .skip(timeline_length - NUMBER_OF_TIMELINE_EVENTS_TO_KEEP_FOR_THE_CACHE)
                .cloned()
                .collect::<Vec<_>>()
                .into()
        } else {
            (*timeline_queue).clone()
        };

        Self { room_id: value.inner.room_id.clone(), inner, timeline_queue }
    }
}

#[cfg(test)]
mod tests {
    use imbl::vector;
    use matrix_sdk_base::deserialized_responses::TimelineEvent;
    use ruma::{
        api::client::sync::sync_events::v4, events::room::message::RoomMessageEventContent,
        room_id, uint, RoomId,
    };
    use serde_json::json;
    use wiremock::MockServer;

    use super::*;
    use crate::test_utils::logged_in_client;

    macro_rules! room_response {
        ( $( $json:tt )+ ) => {
            serde_json::from_value::<v4::SlidingSyncRoom>(
                json!( $( $json )+ )
            ).unwrap()
        };
    }

    async fn new_room(room_id: &RoomId, inner: v4::SlidingSyncRoom) -> SlidingSyncRoom {
        new_room_with_timeline(room_id, inner, vec![]).await
    }

    async fn new_room_with_timeline(
        room_id: &RoomId,
        inner: v4::SlidingSyncRoom,
        timeline: Vec<SyncTimelineEvent>,
    ) -> SlidingSyncRoom {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        SlidingSyncRoom::new(client, room_id.to_owned(), inner, timeline)
    }

    #[tokio::test]
    async fn test_state_from_not_loaded() {
        let mut room = new_room(room_id!("!foo:bar.org"), room_response!({})).await;

        assert_eq!(room.state(), SlidingSyncRoomState::NotLoaded);

        // Update with an empty response, but it doesn't matter.
        room.update(room_response!({}), vec![]);

        assert_eq!(room.state(), SlidingSyncRoomState::Loaded);
    }

    #[tokio::test]
    async fn test_state_from_preloaded() {
        let mut room = new_room(room_id!("!foo:bar.org"), room_response!({})).await;

        room.set_state(SlidingSyncRoomState::Preloaded);

        // Update with an empty response, but it doesn't matter.
        room.update(room_response!({}), vec![]);

        assert_eq!(room.state(), SlidingSyncRoomState::Loaded);
    }

    #[tokio::test]
    async fn test_room_room_id() {
        let room_id = room_id!("!foo:bar.org");
        let room = new_room(room_id, room_response!({})).await;

        assert_eq!(room.room_id(), room_id);
    }

    macro_rules! test_getters {
        (
            $(
                $test_name:ident {
                    $getter:ident () $( . $getter_field:ident )? = $default_value:expr;
                    receives $room_response:expr;
                    _ = $init_or_updated_value:expr;
                    receives nothing;
                    _ = $no_update_value:expr;
                }
            )+
        ) => {
            $(
                #[tokio::test]
                async fn $test_name () {
                    // Default value.
                    {
                        let room = new_room(room_id!("!foo:bar.org"), room_response!({})).await;

                        assert_eq!(room.$getter() $( . $getter_field )?, $default_value, "default value");
                    }

                    // Some value when initializing.
                    {
                        let room = new_room(room_id!("!foo:bar.org"), $room_response).await;

                        assert_eq!(room.$getter() $( . $getter_field )?, $init_or_updated_value, "init value");
                    }

                    // Some value when updating.
                    {

                        let mut room = new_room(room_id!("!foo:bar.org"), room_response!({})).await;

                        // Value is set to the default value.
                        assert_eq!(room.$getter() $( . $getter_field )?, $default_value, "default value (bis)");

                        room.update($room_response, vec![]);

                        // Value has been updated.
                        assert_eq!(room.$getter() $( . $getter_field )?, $init_or_updated_value, "updated value");

                        room.update(room_response!({}), vec![]);

                        // Value is kept.
                        assert_eq!(room.$getter() $( . $getter_field )?, $no_update_value, "not updated value");
                    }

                }
            )+
        };
    }

    test_getters! {
        test_room_name {
            name() = None;
            receives room_response!({"name": "gordon"});
            _ = Some("gordon".to_owned());
            receives nothing;
            _ = Some("gordon".to_owned());
        }

        test_room_is_dm {
            is_dm() = None;
            receives room_response!({"is_dm": true});
            _ = Some(true);
            receives nothing;
            _ = Some(true);
        }

        test_room_is_initial_response {
            is_initial_response() = None;
            receives room_response!({"initial": true});
            _ = Some(true);
            receives nothing;
            _ = Some(true);
        }

        test_has_unread_notifications_with_notification_count {
            has_unread_notifications() = false;
            receives room_response!({"notification_count": 42});
            _ = true;
            receives nothing;
            _ = false;
        }

        test_has_unread_notifications_with_highlight_count {
            has_unread_notifications() = false;
            receives room_response!({"highlight_count": 42});
            _ = true;
            receives nothing;
            _ = false;
        }

        test_unread_notifications_with_notification_count {
            unread_notifications().notification_count = None;
            receives room_response!({"notification_count": 42});
            _ = Some(uint!(42));
            receives nothing;
            _ = None;
        }

        test_unread_notifications_with_highlight_count {
            unread_notifications().highlight_count = None;
            receives room_response!({"highlight_count": 42});
            _ = Some(uint!(42));
            receives nothing;
            _ = None;
        }
    }

    #[tokio::test]
    async fn test_prev_batch() {
        // Default value.
        {
            let room = new_room(room_id!("!foo:bar.org"), room_response!({})).await;

            assert_eq!(room.prev_batch(), None);
        }

        // Some value when initializing.
        {
            let room =
                new_room(room_id!("!foo:bar.org"), room_response!({"prev_batch": "t111_222_333"}))
                    .await;

            assert_eq!(room.prev_batch(), Some("t111_222_333".to_owned()));
        }

        // Some value when updating.
        {
            let mut room = new_room(room_id!("!foo:bar.org"), room_response!({})).await;

            assert_eq!(room.prev_batch(), None);

            room.update(room_response!({"prev_batch": "t111_222_333"}), vec![]);
            assert_eq!(room.prev_batch(), Some("t111_222_333".to_owned()));

            room.update(room_response!({}), vec![]);
            assert_eq!(room.prev_batch(), Some("t111_222_333".to_owned()));
        }
    }

    #[tokio::test]
    async fn test_required_state() {
        // Default value.
        {
            let room = new_room(room_id!("!foo:bar.org"), room_response!({})).await;

            assert!(room.required_state().is_empty());
        }

        // Some value when initializing.
        {
            let room = new_room(
                room_id!("!foo:bar.org"),
                room_response!({
                    "required_state": [
                        {
                            "sender": "@alice:example.com",
                            "type": "m.room.join_rules",
                            "state_key": "",
                            "content": {
                                "join_rule": "invite"
                            }
                        }
                    ]
                }),
            )
            .await;

            assert!(!room.required_state().is_empty());
        }

        // Some value when updating.
        {
            let mut room = new_room(room_id!("!foo:bar.org"), room_response!({})).await;

            assert!(room.required_state().is_empty());

            room.update(
                room_response!({
                    "required_state": [
                        {
                            "sender": "@alice:example.com",
                            "type": "m.room.join_rules",
                            "state_key": "",
                            "content": {
                                "join_rule": "invite"
                            }
                        }
                    ]
                }),
                vec![],
            );
            assert!(!room.required_state().is_empty());

            room.update(room_response!({}), vec![]);
            assert!(!room.required_state().is_empty());
        }
    }

    #[tokio::test]
    async fn test_timeline_queue_initially_empty() {
        let room = new_room(room_id!("!foo:bar.org"), room_response!({})).await;

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

    #[tokio::test]
    async fn test_timeline_queue_initially_not_empty() {
        let room = new_room_with_timeline(
            room_id!("!foo:bar.org"),
            room_response!({}),
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

    #[tokio::test]
    async fn test_timeline_queue_update_with_empty_timeline() {
        let mut room = new_room_with_timeline(
            room_id!("!foo:bar.org"),
            room_response!({}),
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

    #[tokio::test]
    async fn test_timeline_queue_update_with_empty_timeline_and_with_limited() {
        let mut room = new_room_with_timeline(
            room_id!("!foo:bar.org"),
            room_response!({}),
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

    #[tokio::test]
    async fn test_timeline_queue_update_from_preloaded() {
        let mut room = new_room_with_timeline(
            room_id!("!foo:bar.org"),
            room_response!({}),
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

    #[tokio::test]
    async fn test_timeline_queue_update_from_not_loaded() {
        let mut room = new_room_with_timeline(
            room_id!("!foo:bar.org"),
            room_response!({}),
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

    #[tokio::test]
    async fn test_timeline_queue_update_from_not_loaded_with_limited() {
        let mut room = new_room_with_timeline(
            room_id!("!foo:bar.org"),
            room_response!({}),
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
        let frozen_room = FrozenSlidingSyncRoom {
            room_id: room_id!("!29fhd83h92h0:example.com").to_owned(),
            inner: v4::SlidingSyncRoom::default(),
            timeline_queue: vector![TimelineEvent::new(
                Raw::new(&json!({
                    "content": RoomMessageEventContent::text_plain("let it gooo!"),
                    "type": "m.room.message",
                    "event_id": "$xxxxx:example.org",
                    "room_id": "!someroom:example.com",
                    "origin_server_ts": 2189,
                    "sender": "@bob:example.com",
                }))
                .unwrap()
                .cast(),
            )
            .into()],
        };

        assert_eq!(
            serde_json::to_value(&frozen_room).unwrap(),
            json!({
                "room_id": "!29fhd83h92h0:example.com",
                "inner": {},
                "timeline": [
                    {
                        "event": {
                            "content": {
                                "body": "let it gooo!",
                                "msgtype": "m.text"
                            },
                            "event_id": "$xxxxx:example.org",
                            "origin_server_ts": 2189,
                            "room_id": "!someroom:example.com",
                            "sender": "@bob:example.com",
                            "type": "m.room.message"
                        },
                        "encryption_info": null
                    }
                ]
            })
        );
    }

    #[tokio::test]
    async fn test_frozen_sliding_sync_room_has_a_capped_version_of_the_timeline() {
        // Just below the limit.
        {
            let max = NUMBER_OF_TIMELINE_EVENTS_TO_KEEP_FOR_THE_CACHE - 1;
            let timeline_events = (0..=max)
                .map(|nth| {
                    TimelineEvent::new(
                        Raw::new(&json!({
                            "content": RoomMessageEventContent::text_plain(format!("message {nth}")),
                            "type": "m.room.message",
                            "event_id": format!("$x{nth}:baz.org"),
                            "room_id": "!foo:bar.org",
                            "origin_server_ts": nth,
                            "sender": "@alice:baz.org",
                        }))
                        .unwrap()
                        .cast(),
                    )
                    .into()
                })
                .collect::<Vec<_>>();

            let room = new_room_with_timeline(
                room_id!("!foo:bar.org"),
                room_response!({}),
                timeline_events,
            )
            .await;

            let frozen_room = FrozenSlidingSyncRoom::from(&room);
            assert_eq!(frozen_room.timeline_queue.len(), max + 1);
            // Check that the last event is the last event of the timeline, i.e. we only
            // keep the _latest_ events, not the _first_ events.
            assert_eq!(
                frozen_room.timeline_queue.last().unwrap().event.deserialize().unwrap().event_id(),
                &format!("$x{max}:baz.org")
            );
        }

        // Above the limit.
        {
            let max = NUMBER_OF_TIMELINE_EVENTS_TO_KEEP_FOR_THE_CACHE + 2;
            let timeline_events = (0..=max)
                .map(|nth| {
                    TimelineEvent::new(
                    Raw::new(&json!({
                        "content": RoomMessageEventContent::text_plain(format!("message {nth}")),
                        "type": "m.room.message",
                        "event_id": format!("$x{nth}:baz.org"),
                        "room_id": "!foo:bar.org",
                        "origin_server_ts": nth,
                        "sender": "@alice:baz.org",
                    }))
                    .unwrap()
                    .cast(),
                )
                .into()
                })
                .collect::<Vec<_>>();

            let room = new_room_with_timeline(
                room_id!("!foo:bar.org"),
                room_response!({}),
                timeline_events,
            )
            .await;

            let frozen_room = FrozenSlidingSyncRoom::from(&room);
            assert_eq!(
                frozen_room.timeline_queue.len(),
                NUMBER_OF_TIMELINE_EVENTS_TO_KEEP_FOR_THE_CACHE
            );
            // Check that the last event is the last event of the timeline, i.e. we only
            // keep the _latest_ events, not the _first_ events.
            assert_eq!(
                frozen_room.timeline_queue.last().unwrap().event.deserialize().unwrap().event_id(),
                &format!("$x{max}:baz.org")
            );
        }
    }
}
