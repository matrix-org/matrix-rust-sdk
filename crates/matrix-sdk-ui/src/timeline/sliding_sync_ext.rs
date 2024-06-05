// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use async_trait::async_trait;
use matrix_sdk::SlidingSyncRoom;
use tracing::instrument;

use super::EventTimelineItem;

#[async_trait]
pub trait SlidingSyncRoomExt {
    /// Get the latest timeline item of this room, if it is already cached.
    ///
    /// Use `Timeline::latest_event` instead if you already have a timeline for
    /// this `SlidingSyncRoom`.
    async fn latest_timeline_item(&self) -> Option<EventTimelineItem>;
}

#[async_trait]
impl SlidingSyncRoomExt for SlidingSyncRoom {
    /// Get a timeline item representing the latest event in this room.
    /// This method wraps latest_event, converting the event into an
    /// EventTimelineItem.
    #[instrument(skip_all)]
    async fn latest_timeline_item(&self) -> Option<EventTimelineItem> {
        let latest_event =
            self.client().get_room(self.room_id()).and_then(|room| room.latest_event())?;

        EventTimelineItem::from_latest_event(self.client(), self.room_id(), latest_event).await
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use matrix_sdk::{test_utils::logged_in_client, Client, SlidingSyncRoom};
    use matrix_sdk_base::deserialized_responses::SyncTimelineEvent;
    use matrix_sdk_test::async_test;
    use ruma::{
        api::client::sync::sync_events::v4,
        events::room::message::{MessageFormat, MessageType},
        room_id,
        serde::Raw,
        user_id, RoomId, UInt, UserId,
    };
    use serde_json::json;

    use crate::timeline::{SlidingSyncRoomExt, TimelineDetails};

    #[async_test]
    async fn test_initially_latest_message_event_is_none() {
        // Given a room with no latest event
        let room_id = room_id!("!r:x.uk").to_owned();
        let client = logged_in_client(None).await;
        let room = SlidingSyncRoom::new(client, room_id, None, Vec::new());

        // When we ask for the latest event, it is None
        assert!(room.latest_timeline_item().await.is_none());
    }

    #[async_test]
    async fn test_latest_message_event_is_wrapped_as_a_timeline_item() {
        // Given a room exists, and an event came in through a sync
        let room_id = room_id!("!r:x.uk");
        let user_id = user_id!("@s:o.uk");
        let client = logged_in_client(None).await;
        let event = message_event(room_id, user_id, "**My msg**", "<b>My msg</b>", 122343);
        process_event_via_sync_test_helper(room_id, event, &client).await;

        // When we ask for the latest event in the room
        let room = SlidingSyncRoom::new(client.clone(), room_id.to_owned(), None, Vec::new());
        let actual = room.latest_timeline_item().await.unwrap();

        // Then it is wrapped as an EventTimelineItem
        assert_eq!(actual.sender, user_id);
        assert_matches!(actual.sender_profile, TimelineDetails::Unavailable);
        assert_eq!(actual.timestamp.0, UInt::new(122343).unwrap());
        if let MessageType::Text(txt) = actual.content.as_message().unwrap().msgtype() {
            assert_eq!(txt.body, "**My msg**");
            let formatted = txt.formatted.as_ref().unwrap();
            assert_eq!(formatted.format, MessageFormat::Html);
            assert_eq!(formatted.body, "<b>My msg</b>");
        } else {
            panic!("Unexpected message type");
        }
    }

    async fn process_event_via_sync_test_helper(
        room_id: &RoomId,
        event: SyncTimelineEvent,
        client: &Client,
    ) {
        let mut room = v4::SlidingSyncRoom::new();
        room.timeline.push(event.event);

        let mut response = v4::Response::new("6".to_owned());
        response.rooms.insert(room_id.to_owned(), room);

        client.process_sliding_sync_test_helper(&response).await.unwrap();
    }

    fn message_event(
        room_id: &RoomId,
        user_id: &UserId,
        body: &str,
        formatted_body: &str,
        ts: u64,
    ) -> SyncTimelineEvent {
        SyncTimelineEvent::new(
            Raw::from_json_string(
                json!({
                    "event_id": "$eventid6",
                    "sender": user_id,
                    "origin_server_ts": ts,
                    "type": "m.room.message",
                    "room_id": room_id.to_string(),
                    "content": {
                        "body": body,
                        "format": "org.matrix.custom.html",
                        "formatted_body": formatted_body,
                        "msgtype": "m.text"
                    },
                })
                .to_string(),
            )
            .unwrap(),
        )
    }
}
