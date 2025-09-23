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

use bitflags::bitflags;
use ruma::events::{AnyRoomAccountDataEvent, RoomAccountDataEventType, tag::Tags};
use serde::{Deserialize, Serialize};

use super::Room;
use crate::store::Result as StoreResult;

impl Room {
    /// Get the `Tags` for this room.
    pub async fn tags(&self) -> StoreResult<Option<Tags>> {
        if let Some(AnyRoomAccountDataEvent::Tag(event)) = self
            .store
            .get_room_account_data_event(self.room_id(), RoomAccountDataEventType::Tag)
            .await?
            .and_then(|raw| raw.deserialize().ok())
        {
            Ok(Some(event.content.tags))
        } else {
            Ok(None)
        }
    }

    /// Check whether the room is marked as favourite.
    ///
    /// A room is considered favourite if it has received the `m.favourite` tag.
    pub fn is_favourite(&self) -> bool {
        self.info.read().base_info.notable_tags.contains(RoomNotableTags::FAVOURITE)
    }

    /// Check whether the room is marked as low priority.
    ///
    /// A room is considered low priority if it has received the `m.lowpriority`
    /// tag.
    pub fn is_low_priority(&self) -> bool {
        self.info.read().base_info.notable_tags.contains(RoomNotableTags::LOW_PRIORITY)
    }
}

bitflags! {
    /// Notable tags, i.e. subset of tags that we are more interested by.
    ///
    /// We are not interested by all the tags. Some tags are more important than
    /// others, and this struct describes them.
    #[repr(transparent)]
    #[derive(Debug, Default, Clone, Copy, Deserialize, Serialize)]
    pub(crate) struct RoomNotableTags: u8 {
        /// The `m.favourite` tag.
        const FAVOURITE = 0b0000_0001;

        /// THe `m.lowpriority` tag.
        const LOW_PRIORITY = 0b0000_0010;
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use matrix_sdk_test::async_test;
    use ruma::{
        events::tag::{TagInfo, TagName, Tags},
        room_id,
        serde::Raw,
        user_id,
    };
    use serde_json::json;
    use stream_assert::{assert_pending, assert_ready};

    use super::{super::BaseRoomInfo, RoomNotableTags};
    use crate::{
        BaseClient, RoomState, SessionMeta,
        client::ThreadingSupport,
        response_processors as processors,
        store::{RoomLoadSettings, StoreConfig},
    };

    #[async_test]
    async fn test_is_favourite() {
        // Given a room,
        let client = BaseClient::new(
            StoreConfig::new("cross-process-store-locks-holder-name".to_owned()),
            ThreadingSupport::Disabled,
        );

        client
            .activate(
                SessionMeta {
                    user_id: user_id!("@alice:example.org").into(),
                    device_id: ruma::device_id!("AYEAYEAYE").into(),
                },
                RoomLoadSettings::default(),
                #[cfg(feature = "e2e-encryption")]
                None,
            )
            .await
            .unwrap();

        let room_id = room_id!("!test:localhost");
        let room = client.get_or_create_room(room_id, RoomState::Joined);

        // Sanity checks to ensure the room isn't marked as favourite.
        assert!(room.is_favourite().not());

        // Subscribe to the `RoomInfo`.
        let mut room_info_subscriber = room.subscribe_info();

        assert_pending!(room_info_subscriber);

        // Create the tag.
        let tag_raw = Raw::new(&json!({
            "content": {
                "tags": {
                    "m.favourite": {
                        "order": 0.0
                    },
                },
            },
            "type": "m.tag",
        }))
        .unwrap()
        .cast_unchecked();

        // When the new tag is handled and applied.
        let mut context = processors::Context::default();

        processors::account_data::for_room(&mut context, room_id, &[tag_raw], &client.state_store);

        processors::changes::save_and_apply(
            context.clone(),
            &client.state_store,
            &client.ignore_user_list_changes,
            None,
        )
        .await
        .unwrap();

        // The `RoomInfo` is getting notified.
        assert_ready!(room_info_subscriber);
        assert_pending!(room_info_subscriber);

        // The room is now marked as favourite.
        assert!(room.is_favourite());

        // Now, let's remove the tag.
        let tag_raw = Raw::new(&json!({
            "content": {
                "tags": {},
            },
            "type": "m.tag"
        }))
        .unwrap()
        .cast_unchecked();

        processors::account_data::for_room(&mut context, room_id, &[tag_raw], &client.state_store);

        processors::changes::save_and_apply(
            context,
            &client.state_store,
            &client.ignore_user_list_changes,
            None,
        )
        .await
        .unwrap();

        // The `RoomInfo` is getting notified.
        assert_ready!(room_info_subscriber);
        assert_pending!(room_info_subscriber);

        // The room is now marked as _not_ favourite.
        assert!(room.is_favourite().not());
    }

    #[async_test]
    async fn test_is_low_priority() {
        // Given a room,
        let client = BaseClient::new(
            StoreConfig::new("cross-process-store-locks-holder-name".to_owned()),
            ThreadingSupport::Disabled,
        );

        client
            .activate(
                SessionMeta {
                    user_id: user_id!("@alice:example.org").into(),
                    device_id: ruma::device_id!("AYEAYEAYE").into(),
                },
                RoomLoadSettings::default(),
                #[cfg(feature = "e2e-encryption")]
                None,
            )
            .await
            .unwrap();

        let room_id = room_id!("!test:localhost");
        let room = client.get_or_create_room(room_id, RoomState::Joined);

        // Sanity checks to ensure the room isn't marked as low priority.
        assert!(!room.is_low_priority());

        // Subscribe to the `RoomInfo`.
        let mut room_info_subscriber = room.subscribe_info();

        assert_pending!(room_info_subscriber);

        // Create the tag.
        let tag_raw = Raw::new(&json!({
            "content": {
                "tags": {
                    "m.lowpriority": {
                        "order": 0.0
                    },
                }
            },
            "type": "m.tag"
        }))
        .unwrap()
        .cast_unchecked();

        // When the new tag is handled and applied.
        let mut context = processors::Context::default();

        processors::account_data::for_room(&mut context, room_id, &[tag_raw], &client.state_store);

        processors::changes::save_and_apply(
            context.clone(),
            &client.state_store,
            &client.ignore_user_list_changes,
            None,
        )
        .await
        .unwrap();

        // The `RoomInfo` is getting notified.
        assert_ready!(room_info_subscriber);
        assert_pending!(room_info_subscriber);

        // The room is now marked as low priority.
        assert!(room.is_low_priority());

        // Now, let's remove the tag.
        let tag_raw = Raw::new(&json!({
            "content": {
                "tags": {},
            },
            "type": "m.tag"
        }))
        .unwrap()
        .cast_unchecked();

        processors::account_data::for_room(&mut context, room_id, &[tag_raw], &client.state_store);

        processors::changes::save_and_apply(
            context,
            &client.state_store,
            &client.ignore_user_list_changes,
            None,
        )
        .await
        .unwrap();

        // The `RoomInfo` is getting notified.
        assert_ready!(room_info_subscriber);
        assert_pending!(room_info_subscriber);

        // The room is now marked as _not_ low priority.
        assert!(room.is_low_priority().not());
    }

    #[test]
    fn test_handle_notable_tags_favourite() {
        let mut base_room_info = BaseRoomInfo::default();

        let mut tags = Tags::new();
        tags.insert(TagName::Favorite, TagInfo::default());

        assert!(base_room_info.notable_tags.contains(RoomNotableTags::FAVOURITE).not());
        base_room_info.handle_notable_tags(&tags);
        assert!(base_room_info.notable_tags.contains(RoomNotableTags::FAVOURITE));
        tags.clear();
        base_room_info.handle_notable_tags(&tags);
        assert!(base_room_info.notable_tags.contains(RoomNotableTags::FAVOURITE).not());
    }

    #[test]
    fn test_handle_notable_tags_low_priority() {
        let mut base_room_info = BaseRoomInfo::default();

        let mut tags = Tags::new();
        tags.insert(TagName::LowPriority, TagInfo::default());

        assert!(base_room_info.notable_tags.contains(RoomNotableTags::LOW_PRIORITY).not());
        base_room_info.handle_notable_tags(&tags);
        assert!(base_room_info.notable_tags.contains(RoomNotableTags::LOW_PRIORITY));
        tags.clear();
        base_room_info.handle_notable_tags(&tags);
        assert!(base_room_info.notable_tags.contains(RoomNotableTags::LOW_PRIORITY).not());
    }
}
