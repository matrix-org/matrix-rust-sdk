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

    /// The latest event represents an invite to a room.
    RemoteInvite {
        /// The timestamp of the invite.
        timestamp: MilliSecondsSinceUnixEpoch,

        /// The inviter (can be unknown).
        inviter: Option<OwnedUserId>,

        /// The inviter's profile (can be unknown).
        inviter_profile: TimelineDetails<Profile>,
    },

    /// The latest event represents a local event that is sending, or that
    /// cannot be sent, either because a previous local event, or this local
    /// event cannot be sent.
    Local {
        /// The timestamp of the local event.
        timestamp: MilliSecondsSinceUnixEpoch,

        /// The sender of the remote event.
        sender: OwnedUserId,

        /// The sender's profile.
        profile: TimelineDetails<Profile>,

        /// The content of the local event.
        content: TimelineItemContent,

        /// Whether the local event is sending, has been sent or cannot be sent.
        state: LatestEventValueLocalState,
    },
}

#[derive(Debug)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum LatestEventValueLocalState {
    IsSending,
    HasBeenSent,
    CannotBeSent,
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
                let Some(timestamp) = timeline_event.timestamp() else {
                    return Self::None;
                };
                let Some(sender) = timeline_event.sender() else {
                    return Self::None;
                };
                let is_own = client.user_id().map(|user_id| user_id == sender).unwrap_or(false);

                let profile =
                    TimelineDetails::from_initial_value(Profile::load(room, &sender).await);

                match TimelineItemContent::from_event(room, timeline_event).await {
                    Some(content) => Self::Remote { timestamp, sender, is_own, profile, content },
                    None => Self::None,
                }
            }
            BaseLatestEventValue::RemoteInvite { timestamp, inviter, .. } => {
                let inviter_profile = if let Some(inviter_id) = &inviter {
                    TimelineDetails::from_initial_value(Profile::load(room, inviter_id).await)
                } else {
                    TimelineDetails::Unavailable
                };

                Self::RemoteInvite { timestamp, inviter, inviter_profile }
            }
            BaseLatestEventValue::LocalIsSending(ref local_value)
            | BaseLatestEventValue::LocalHasBeenSent { value: ref local_value, .. }
            | BaseLatestEventValue::LocalCannotBeSent(ref local_value) => {
                let LocalLatestEventValue { timestamp, content: serialized_content } = local_value;

                let Ok(message_like_event_content) = serialized_content.deserialize() else {
                    return Self::None;
                };

                let sender =
                    client.user_id().expect("The `Client` is supposed to be logged").to_owned();
                let profile =
                    TimelineDetails::from_initial_value(Profile::load(room, &sender).await);

                match TimelineAction::from_content(message_like_event_content, None, None, None) {
                    TimelineAction::AddItem { content } => Self::Local {
                        timestamp: *timestamp,
                        sender,
                        profile,
                        content,
                        state: match value {
                            BaseLatestEventValue::LocalIsSending(_) => {
                                LatestEventValueLocalState::IsSending
                            }
                            BaseLatestEventValue::LocalHasBeenSent { .. } => {
                                LatestEventValueLocalState::HasBeenSent
                            }
                            BaseLatestEventValue::LocalCannotBeSent(_) => {
                                LatestEventValueLocalState::CannotBeSent
                            }
                            BaseLatestEventValue::Remote(_)
                            | BaseLatestEventValue::RemoteInvite { .. }
                            | BaseLatestEventValue::None => {
                                unreachable!("Only local latest events are supposed to be handled");
                            }
                        },
                    },

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
    use std::{ops::Not, time::Duration};

    use assert_matches::assert_matches;
    use matrix_sdk::{
        latest_events::{LocalLatestEventValue, RemoteLatestEventValue},
        store::SerializableEventContent,
        test_utils::mocks::MatrixMockServer,
    };
    use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
    use ruma::{
        MilliSecondsSinceUnixEpoch, event_id,
        events::{AnyMessageLikeEventContent, room::message::RoomMessageEventContent},
        owned_event_id, room_id, uint, user_id,
    };

    use super::{
        super::{MsgLikeContent, MsgLikeKind, TimelineItemContent},
        BaseLatestEventValue, LatestEventValue, LatestEventValueLocalState, TimelineDetails,
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
        let event_factory = EventFactory::new();

        let base_value = BaseLatestEventValue::Remote(RemoteLatestEventValue::from_plaintext(
            event_factory
                .server_ts(42)
                .sender(sender)
                .text_msg("raclette")
                .event_id(event_id!("$ev0"))
                .into_raw_sync(),
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
                TimelineItemContent::MsgLike(MsgLikeContent { kind: MsgLikeKind::Message(message), .. }) => {
                    assert_eq!(message.body(), "raclette");
                }
            );
        })
    }

    #[async_test]
    async fn test_remote_invite() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room = server.sync_room(&client, JoinedRoomBuilder::new(room_id!("!r0"))).await;
        let user_id = user_id!("@mnt_io:matrix.org");

        let base_value = BaseLatestEventValue::RemoteInvite {
            event_id: None,
            timestamp: MilliSecondsSinceUnixEpoch(42u32.into()),
            inviter: Some(user_id.to_owned()),
        };
        let value =
            LatestEventValue::from_base_latest_event_value(base_value, &room, &client).await;

        assert_matches!(value, LatestEventValue::RemoteInvite { timestamp, inviter, inviter_profile} => {
            assert_eq!(u64::from(timestamp.get()), 42u64);
            assert_eq!(inviter.as_deref(), Some(user_id));
            assert_matches!(inviter_profile, TimelineDetails::Unavailable);
        })
    }

    #[async_test]
    async fn test_local_is_sending() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room = server.sync_room(&client, JoinedRoomBuilder::new(room_id!("!r0"))).await;

        let base_value = BaseLatestEventValue::LocalIsSending(LocalLatestEventValue {
            timestamp: MilliSecondsSinceUnixEpoch(uint!(42)),
            content: SerializableEventContent::new(&AnyMessageLikeEventContent::RoomMessage(
                RoomMessageEventContent::text_plain("raclette"),
            ))
            .unwrap(),
        });
        let value =
            LatestEventValue::from_base_latest_event_value(base_value, &room, &client).await;

        assert_matches!(value, LatestEventValue::Local { timestamp, sender, profile, content, state } => {
            assert_eq!(u64::from(timestamp.get()), 42u64);
            assert_eq!(sender, "@example:localhost");
            assert_matches!(profile, TimelineDetails::Unavailable);
            assert_matches!(
                content,
                TimelineItemContent::MsgLike(MsgLikeContent { kind: MsgLikeKind::Message(_), .. })
            );
            assert_matches!(state, LatestEventValueLocalState::IsSending);
        })
    }

    #[async_test]
    async fn test_local_has_been_sent() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room = server.sync_room(&client, JoinedRoomBuilder::new(room_id!("!r0"))).await;

        let base_value = BaseLatestEventValue::LocalHasBeenSent {
            event_id: owned_event_id!("$ev0"),
            value: LocalLatestEventValue {
                timestamp: MilliSecondsSinceUnixEpoch(uint!(42)),
                content: SerializableEventContent::new(&AnyMessageLikeEventContent::RoomMessage(
                    RoomMessageEventContent::text_plain("raclette"),
                ))
                .unwrap(),
            },
        };
        let value =
            LatestEventValue::from_base_latest_event_value(base_value, &room, &client).await;

        assert_matches!(value, LatestEventValue::Local { timestamp, sender, profile, content, state } => {
            assert_eq!(u64::from(timestamp.get()), 42u64);
            assert_eq!(sender, "@example:localhost");
            assert_matches!(profile, TimelineDetails::Unavailable);
            assert_matches!(
                content,
                TimelineItemContent::MsgLike(MsgLikeContent { kind: MsgLikeKind::Message(_), .. })
            );
            assert_matches!(state, LatestEventValueLocalState::HasBeenSent);
        })
    }

    #[async_test]
    async fn test_local_cannot_be_sent() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room = server.sync_room(&client, JoinedRoomBuilder::new(room_id!("!r0"))).await;

        let base_value = BaseLatestEventValue::LocalCannotBeSent(LocalLatestEventValue {
            timestamp: MilliSecondsSinceUnixEpoch(uint!(42)),
            content: SerializableEventContent::new(&AnyMessageLikeEventContent::RoomMessage(
                RoomMessageEventContent::text_plain("raclette"),
            ))
            .unwrap(),
        });
        let value =
            LatestEventValue::from_base_latest_event_value(base_value, &room, &client).await;

        assert_matches!(value, LatestEventValue::Local { timestamp, sender, profile, content, state } => {
            assert_eq!(u64::from(timestamp.get()), 42u64);
            assert_eq!(sender, "@example:localhost");
            assert_matches!(profile, TimelineDetails::Unavailable);
            assert_matches!(
                content,
                TimelineItemContent::MsgLike(MsgLikeContent { kind: MsgLikeKind::Message(_), .. })
            );
            assert_matches!(state, LatestEventValueLocalState::CannotBeSent);
        })
    }

    #[async_test]
    async fn test_remote_edit() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room = server.sync_room(&client, JoinedRoomBuilder::new(room_id!("!r0"))).await;
        let sender = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new();

        let base_value = BaseLatestEventValue::Remote(RemoteLatestEventValue::from_plaintext(
            event_factory
                .server_ts(42)
                .sender(sender)
                .text_msg("bonjour")
                .event_id(event_id!("$ev1"))
                .edit(event_id!("$ev0"), RoomMessageEventContent::text_plain("fondue").into())
                .into_raw_sync(),
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
                TimelineItemContent::MsgLike(MsgLikeContent { kind: MsgLikeKind::Message(message), .. }) => {
                    assert_eq!(message.body(), "fondue");
                }
            );
        })
    }

    #[async_test]
    async fn test_remote_beacon_stop() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room = server.sync_room(&client, JoinedRoomBuilder::new(room_id!("!r0"))).await;
        let sender = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new();

        let base_value = BaseLatestEventValue::Remote(RemoteLatestEventValue::from_plaintext(
            event_factory
                .server_ts(42)
                .sender(sender)
                .beacon_info(Some("Alice's walk".to_owned()), Duration::from_secs(60), false, None)
                .state_key(sender)
                .event_id(event_id!("$beacon-stop"))
                .into_raw_sync(),
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
                TimelineItemContent::MsgLike(MsgLikeContent { kind: MsgLikeKind::LiveLocation(state), .. }) => {
                    assert!(!state.is_live(), "stop beacon should not be live");
                    assert_eq!(state.description(), Some("Alice's walk"));
                }
            );
        })
    }
}
