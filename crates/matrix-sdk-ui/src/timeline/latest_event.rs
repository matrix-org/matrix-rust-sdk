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

use matrix_sdk::{Client, Room, latest_events::LocalLatestEventValue};
use matrix_sdk_base::latest_event::LatestEventValue as BaseLatestEventValue;
use ruma::{MilliSecondsSinceUnixEpoch, OwnedUserId};
use tracing::trace;

use crate::timeline::{
    Profile, TimelineDetails, TimelineItemContent, event_handler::TimelineAction,
    traits::RoomDataProvider,
};

/// A simplified version of [`matrix_sdk_base::latest_event::LatestEventValue`]
/// tailored for this `timeline` module.
#[derive(Debug)]
pub enum LatestEventValue {
    /// No value has been computed yet, or no candidate value was found.
    None,

    /// The latest event represents a remote event.
    Remote {
        /// The timestamp of the remote event.
        timestamp: MilliSecondsSinceUnixEpoch,

        /// The sender of the remote event.
        sender: OwnedUserId,

        /// Has this event been sent by the current logged user?
        is_own: bool,

        /// The sender's profile.
        profile: TimelineDetails<Profile>,

        /// The content of the remote event.
        content: TimelineItemContent,
    },

    /// The latest event represents a local event that is sending, or that
    /// cannot be sent, either because a previous local event, or this local
    /// event cannot be sent.
    Local {
        /// The timestamp of the local event.
        timestamp: MilliSecondsSinceUnixEpoch,

        /// The content of the local event.
        content: TimelineItemContent,

        /// Whether the local event is sending if it is set to `true`, otherwise
        /// it cannot be sent.
        is_sending: bool,
    },
}

impl LatestEventValue {
    pub(crate) async fn from_base_latest_event_value(
        value: BaseLatestEventValue,
        room: &Room,
        client: &Client,
    ) -> Self {
        match value {
            BaseLatestEventValue::None => Self::None,
            BaseLatestEventValue::Remote(timeline_event) => {
                let raw_any_sync_timeline_event = timeline_event.into_raw();
                let Ok(any_sync_timeline_event) = raw_any_sync_timeline_event.deserialize() else {
                    return Self::None;
                };

                let timestamp = any_sync_timeline_event.origin_server_ts();
                let sender = any_sync_timeline_event.sender().to_owned();
                let is_own = client.user_id().map(|user_id| user_id == sender).unwrap_or(false);
                let profile = room
                    .profile_from_user_id(&sender)
                    .await
                    .map(TimelineDetails::Ready)
                    .unwrap_or(TimelineDetails::Unavailable);

                let Some(TimelineAction::AddItem { content }) = TimelineAction::from_event(
                    any_sync_timeline_event,
                    &raw_any_sync_timeline_event,
                    room,
                    None,
                    None,
                    None,
                    None,
                )
                .await
                else {
                    return Self::None;
                };

                Self::Remote { timestamp, sender, is_own, profile, content }
            }
            BaseLatestEventValue::LocalIsSending(LocalLatestEventValue {
                timestamp,
                content: ref serialized_content,
            })
            | BaseLatestEventValue::LocalCannotBeSent(LocalLatestEventValue {
                timestamp,
                content: ref serialized_content,
            }) => {
                let Ok(message_like_event_content) = serialized_content.deserialize() else {
                    return Self::None;
                };

                let is_sending = matches!(value, BaseLatestEventValue::LocalIsSending(_));

                match TimelineAction::from_content(message_like_event_content, None, None, None) {
                    TimelineAction::AddItem { content } => {
                        Self::Local { timestamp, content, is_sending }
                    }

                    TimelineAction::HandleAggregation { kind, .. } => {
                        // Add some debug logging here to help diagnose issues with the latest
                        // event.
                        trace!("latest event is an aggregation: {}", kind.debug_string());
                        Self::None
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use assert_matches::assert_matches;
    use matrix_sdk::{
        latest_events::{LocalLatestEventValue, RemoteLatestEventValue},
        store::SerializableEventContent,
        test_utils::mocks::MatrixMockServer,
    };
    use matrix_sdk_test::{JoinedRoomBuilder, async_test};
    use ruma::{
        MilliSecondsSinceUnixEpoch,
        events::{AnyMessageLikeEventContent, room::message::RoomMessageEventContent},
        room_id,
        serde::Raw,
        uint, user_id,
    };
    use serde_json::json;

    use super::{
        super::{MsgLikeContent, MsgLikeKind, TimelineItemContent},
        BaseLatestEventValue, LatestEventValue, TimelineDetails,
    };

    #[async_test]
    async fn test_none() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room = server.sync_room(&client, JoinedRoomBuilder::new(room_id!("!r0"))).await;

        let base_value = BaseLatestEventValue::None;
        let value =
            LatestEventValue::from_base_latest_event_value(base_value, &room, &client).await;

        assert_matches!(value, LatestEventValue::None);
    }

    #[async_test]
    async fn test_remote() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room = server.sync_room(&client, JoinedRoomBuilder::new(room_id!("!r0"))).await;
        let sender = user_id!("@mnt_io:matrix.org");

        let base_value = BaseLatestEventValue::Remote(RemoteLatestEventValue::from_plaintext(
            Raw::from_json_string(
                json!({
                    "content": RoomMessageEventContent::text_plain("raclette"),
                    "type": "m.room.message",
                    "event_id": "$ev0",
                    "room_id": "!r0",
                    "origin_server_ts": 42,
                    "sender": sender,
                })
                .to_string(),
            )
            .unwrap(),
        ));
        let value =
            LatestEventValue::from_base_latest_event_value(base_value, &room, &client).await;

        assert_matches!(value, LatestEventValue::Remote { timestamp, sender: received_sender, is_own, profile, content } => {
            assert_eq!(u64::from(timestamp.get()), 42u64);
            assert_eq!(received_sender, sender);
            assert!(is_own.not());
            assert_matches!(profile, TimelineDetails::Unavailable);
            assert_matches!(
                content,
                TimelineItemContent::MsgLike(MsgLikeContent { kind: MsgLikeKind::Message(_), .. })
            );
        })
    }

    #[async_test]
    async fn test_local_is_sending() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room = server.sync_room(&client, JoinedRoomBuilder::new(room_id!("!r0"))).await;

        let base_value = BaseLatestEventValue::LocalIsSending(LocalLatestEventValue {
            timestamp: MilliSecondsSinceUnixEpoch(uint!(42)),
            content: SerializableEventContent::from_raw(
                Raw::new(&AnyMessageLikeEventContent::RoomMessage(
                    RoomMessageEventContent::text_plain("raclette"),
                ))
                .unwrap(),
                "m.room.message".to_owned(),
            ),
        });
        let value =
            LatestEventValue::from_base_latest_event_value(base_value, &room, &client).await;

        assert_matches!(value, LatestEventValue::Local { timestamp, content, is_sending } => {
            assert_eq!(u64::from(timestamp.get()), 42u64);
            assert_matches!(
                content,
                TimelineItemContent::MsgLike(MsgLikeContent { kind: MsgLikeKind::Message(_), .. })
            );
            assert!(is_sending);
        })
    }

    #[async_test]
    async fn test_local_cannot_be_sent() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room = server.sync_room(&client, JoinedRoomBuilder::new(room_id!("!r0"))).await;

        let base_value = BaseLatestEventValue::LocalCannotBeSent(LocalLatestEventValue {
            timestamp: MilliSecondsSinceUnixEpoch(uint!(42)),
            content: SerializableEventContent::from_raw(
                Raw::new(&AnyMessageLikeEventContent::RoomMessage(
                    RoomMessageEventContent::text_plain("raclette"),
                ))
                .unwrap(),
                "m.room.message".to_owned(),
            ),
        });
        let value =
            LatestEventValue::from_base_latest_event_value(base_value, &room, &client).await;

        assert_matches!(value, LatestEventValue::Local { timestamp, content, is_sending } => {
            assert_eq!(u64::from(timestamp.get()), 42u64);
            assert_matches!(
                content,
                TimelineItemContent::MsgLike(MsgLikeContent { kind: MsgLikeKind::Message(_), .. })
            );
            assert!(is_sending.not());
        })
    }
}
