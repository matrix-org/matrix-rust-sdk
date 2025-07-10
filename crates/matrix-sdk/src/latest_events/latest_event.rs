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

use eyeball::{AsyncLock, SharedObservable, Subscriber};
use matrix_sdk_base::event_cache::Event;
use ruma::{
    events::{
        call::{invite::SyncCallInviteEvent, notify::SyncCallNotifyEvent},
        poll::unstable_start::SyncUnstablePollStartEvent,
        relation::RelationType,
        room::{
            member::{MembershipState, SyncRoomMemberEvent},
            message::{MessageType, SyncRoomMessageEvent},
            power_levels::RoomPowerLevels,
        },
        sticker::SyncStickerEvent,
        AnySyncMessageLikeEvent, AnySyncStateEvent, AnySyncTimelineEvent,
    },
    EventId, OwnedEventId, OwnedRoomId, RoomId, UserId,
};
use tracing::warn;

use crate::{event_cache::RoomEventCache, room::WeakRoom};

/// The latest event of a room or a thread.
///
/// Use [`LatestEvent::subscribe`] to get a stream of updates.
#[derive(Debug)]
pub(super) struct LatestEvent {
    /// The room owning this latest event.
    room_id: OwnedRoomId,
    /// The thread (if any) owning this latest event.
    thread_id: Option<OwnedEventId>,
    /// The latest event value.
    value: SharedObservable<LatestEventValue, AsyncLock>,
}

impl LatestEvent {
    pub(super) async fn new(
        room_id: &RoomId,
        thread_id: Option<&EventId>,
        room_event_cache: &RoomEventCache,
        weak_room: &WeakRoom,
    ) -> Self {
        Self {
            room_id: room_id.to_owned(),
            thread_id: thread_id.map(ToOwned::to_owned),
            value: SharedObservable::new_async(
                LatestEventValue::new(room_id, thread_id, room_event_cache, weak_room).await,
            ),
        }
    }

    /// Return a [`Subscriber`] to new values.
    pub async fn subscribe(&self) -> Subscriber<LatestEventValue, AsyncLock> {
        self.value.subscribe().await
    }

    /// Update the inner latest event value.
    pub async fn update(
        &mut self,
        room_event_cache: &RoomEventCache,
        power_levels: &Option<(&UserId, RoomPowerLevels)>,
    ) {
        let new_value = LatestEventValue::new_with_power_levels(
            &self.room_id,
            self.thread_id.as_deref(),
            room_event_cache,
            power_levels,
        )
        .await;

        self.value.set(new_value).await;
    }
}

/// A latest event value!
#[derive(Debug, Default, Clone)]
pub enum LatestEventValue {
    /// No value has been computed yet, or no candidate value was found.
    #[default]
    None,

    /// A `m.room.message` event.
    RoomMessage(SyncRoomMessageEvent),

    /// A `m.sticker` event.
    Sticker(SyncStickerEvent),

    /// An `org.matrix.msc3381.poll.start` event.
    Poll(SyncUnstablePollStartEvent),

    /// A `m.call.invite` event.
    CallInvite(SyncCallInviteEvent),

    /// A `m.call.notify` event.
    CallNotify(SyncCallNotifyEvent),

    /// A `m.room.member` event, more precisely a knock membership change that
    /// can be handled by the current user.
    KnockedStateEvent(SyncRoomMemberEvent),
}

impl LatestEventValue {
    async fn new(
        room_id: &RoomId,
        thread_id: Option<&EventId>,
        room_event_cache: &RoomEventCache,
        weak_room: &WeakRoom,
    ) -> Self {
        // Get the power levels of the user for the current room if the `WeakRoom` is
        // still valid.
        let room = weak_room.get();
        let power_levels = match &room {
            Some(room) => {
                let power_levels = room.power_levels().await.ok();

                Some(room.own_user_id()).zip(power_levels)
            }

            None => None,
        };

        Self::new_with_power_levels(room_id, thread_id, room_event_cache, &power_levels).await
    }

    async fn new_with_power_levels(
        _room_id: &RoomId,
        _thread_id: Option<&EventId>,
        room_event_cache: &RoomEventCache,
        power_levels: &Option<(&UserId, RoomPowerLevels)>,
    ) -> Self {
        room_event_cache
            .rfind_map_event_in_memory_by(|event| find_and_map(event, power_levels))
            .await
            .unwrap_or_default()
    }
}

fn find_and_map(
    event: &Event,
    power_levels: &Option<(&UserId, RoomPowerLevels)>,
) -> Option<LatestEventValue> {
    // Cast the event into an `AnySyncTimelineEvent`. If deserializing fails, we
    // ignore the event.
    let Some(event) = event.raw().deserialize().ok() else {
        warn!(?event, "Failed to deserialize the event when looking for a suitable latest event");

        return None;
    };

    match event {
        AnySyncTimelineEvent::MessageLike(message_like_event) => match message_like_event {
            AnySyncMessageLikeEvent::RoomMessage(message) => {
                if let Some(original_message) = message.as_original() {
                    // Don't show incoming verification requests.
                    if let MessageType::VerificationRequest(_) = original_message.content.msgtype {
                        return None;
                    }

                    // Check if this is a replacement for another message. If it is, ignore it.
                    let is_replacement =
                        original_message.content.relates_to.as_ref().is_some_and(|relates_to| {
                            if let Some(relation_type) = relates_to.rel_type() {
                                relation_type == RelationType::Replacement
                            } else {
                                false
                            }
                        });

                    if is_replacement {
                        None
                    } else {
                        Some(LatestEventValue::RoomMessage(message))
                    }
                } else {
                    Some(LatestEventValue::RoomMessage(message))
                }
            }

            AnySyncMessageLikeEvent::UnstablePollStart(poll) => Some(LatestEventValue::Poll(poll)),

            AnySyncMessageLikeEvent::CallInvite(invite) => {
                Some(LatestEventValue::CallInvite(invite))
            }

            AnySyncMessageLikeEvent::CallNotify(notify) => {
                Some(LatestEventValue::CallNotify(notify))
            }

            AnySyncMessageLikeEvent::Sticker(sticker) => Some(LatestEventValue::Sticker(sticker)),

            // Encrypted events are not suitable.
            AnySyncMessageLikeEvent::RoomEncrypted(_) => None,

            // Everything else is considered not suitable.
            _ => None,
        },

        // We don't currently support most state events
        AnySyncTimelineEvent::State(state) => {
            // But we make an exception for knocked state events *if* the current user
            // can either accept or decline them.
            if let AnySyncStateEvent::RoomMember(member) = state {
                if matches!(member.membership(), MembershipState::Knock) {
                    let can_accept_or_decline_knocks = match power_levels {
                        Some((own_user_id, room_power_levels)) => {
                            room_power_levels.user_can_invite(own_user_id)
                                || room_power_levels.user_can_kick(own_user_id)
                        }
                        _ => false,
                    };

                    // The current user can act on the knock changes, so they should be
                    // displayed
                    if can_accept_or_decline_knocks {
                        return Some(LatestEventValue::KnockedStateEvent(member));
                    }
                }
            }

            None
        }
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use matrix_sdk_test::event_factory::EventFactory;
    use ruma::{event_id, user_id};

    use super::{find_and_map, LatestEventValue};

    macro_rules! assert_latest_event_value {
        ( with | $event_factory:ident | $event_builder:block
          it produces $match:pat ) => {
            let user_id = user_id!("@mnt_io:matrix.org");
            let event_factory = EventFactory::new().sender(user_id);
            let event = {
                let $event_factory = event_factory;
                $event_builder
            };

            assert_matches!(find_and_map(&event, &None), $match);
        };
    }

    #[test]
    fn test_latest_event_value_room_message() {
        assert_latest_event_value!(
            with |event_factory| {
                event_factory.text_msg("hello").into_event()
            }
            it produces Some(LatestEventValue::RoomMessage(_))
        );
    }

    #[test]
    fn test_latest_event_value_room_message_redacted() {
        assert_latest_event_value!(
            with |event_factory| {
                event_factory
                    .redacted(
                        user_id!("@mnt_io:matrix.org"),
                        ruma::events::room::message::RedactedRoomMessageEventContent::new()
                    )
                    .into_event()
            }
            it produces Some(LatestEventValue::RoomMessage(_))
        );
    }

    #[test]
    fn test_latest_event_value_room_message_replacement() {
        assert_latest_event_value!(
            with |event_factory| {
                event_factory
                    .text_msg("bonjour")
                    .edit(
                        event_id!("$ev0"),
                        ruma::events::room::message::RoomMessageEventContent::text_plain("hello").into()
                    )
                    .into_event()
            }
            it produces None
        );
    }

    #[test]
    fn test_latest_event_value_poll() {
        assert_latest_event_value!(
            with |event_factory| {
                event_factory
                    .poll_start(
                        "the people need to know",
                        "comté > gruyère",
                        vec!["yes", "oui"]
                    )
                    .into_event()
            }
            it produces Some(LatestEventValue::Poll(_))
        );
    }

    #[test]
    fn test_latest_event_value_call_invite() {
        assert_latest_event_value!(
            with |event_factory| {
                event_factory
                    .call_invite(
                        ruma::OwnedVoipId::from("vvooiipp".to_owned()),
                        ruma::UInt::from(1234u32),
                        ruma::events::call::SessionDescription::new("type".to_owned(), "sdp".to_owned()),
                        ruma::VoipVersionId::V1,
                    )
                    .into_event()
            }
            it produces Some(LatestEventValue::CallInvite(_))
        );
    }

    #[test]
    fn test_latest_event_value_call_notify() {
        assert_latest_event_value!(
            with |event_factory| {
                event_factory
                    .call_notify(
                        "call_id".to_owned(),
                        ruma::events::call::notify::ApplicationType::Call,
                        ruma::events::call::notify::NotifyType::Ring,
                        ruma::events::Mentions::new(),
                    )
                    .into_event()
            }
            it produces Some(LatestEventValue::CallNotify(_))
        );
    }

    #[test]
    fn test_latest_event_value_sticker() {
        assert_latest_event_value!(
            with |event_factory| {
                event_factory
                    .sticker(
                        "wink wink",
                        ruma::events::room::ImageInfo::new(),
                        ruma::OwnedMxcUri::from("mxc://foo/bar")
                    )
                    .into_event()
            }
            it produces Some(LatestEventValue::Sticker(_))
        );
    }

    #[test]
    fn test_latest_event_value_encrypted_room_message() {
        assert_latest_event_value!(
            with |event_factory| {
                event_factory
                    .event(ruma::events::room::encrypted::RoomEncryptedEventContent::new(
                        ruma::events::room::encrypted::EncryptedEventScheme::MegolmV1AesSha2(
                            ruma::events::room::encrypted::MegolmV1AesSha2ContentInit {
                                ciphertext: "cipher".to_owned(),
                                sender_key: "sender_key".to_owned(),
                                device_id: "device_id".into(),
                                session_id: "session_id".to_owned(),
                            }
                            .into(),
                        ),
                        None,
                    ))
                    .into_event()
            }
            it produces None
        );
    }

    #[test]
    fn test_latest_event_value_reaction() {
        // Take a random message-like event.
        assert_latest_event_value!(
            with |event_factory| {
                event_factory
                    .reaction(event_id!("$ev0"), "+1")
                    .into_event()
            }
            it produces None
        );
    }

    #[test]
    fn test_latest_event_state_event() {
        assert_latest_event_value!(
            with |event_factory| {
                event_factory
                    .room_topic("new room topic")
                    .into_event()
            }
            it produces None
        );
    }

    #[test]
    fn test_latest_event_knocked_state_event_without_power_levels() {
        assert_latest_event_value!(
            with |event_factory| {
                event_factory
                    .member(user_id!("@other_mnt_io:server.name"))
                    .membership(ruma::events::room::member::MembershipState::Knock)
                    .into_event()
            }
            it produces None
        );
    }

    #[test]
    fn test_latest_event_knocked_state_event_with_power_levels() {
        use ruma::events::room::power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent};

        let user_id = user_id!("@mnt_io:matrix.org");
        let other_user_id = user_id!("@other_mnt_io:server.name");
        let event_factory = EventFactory::new().sender(user_id);
        let event = event_factory
            .member(other_user_id)
            .membership(ruma::events::room::member::MembershipState::Knock)
            .into_event();

        let room_power_levels_event = RoomPowerLevelsEventContent::new();
        let mut room_power_levels = RoomPowerLevels::from(room_power_levels_event);
        room_power_levels.users_default = 5.into();

        // Cannot accept. Cannot decline.
        {
            let mut room_power_levels = room_power_levels.clone();
            room_power_levels.invite = 10.into();
            room_power_levels.kick = 10.into();
            assert_matches!(
                find_and_map(&event, &Some((user_id, room_power_levels))),
                None,
                "cannot accept, cannot decline",
            );
        }

        // Can accept. Cannot decline.
        {
            let mut room_power_levels = room_power_levels.clone();
            room_power_levels.invite = 0.into();
            room_power_levels.kick = 10.into();
            assert_matches!(
                find_and_map(&event, &Some((user_id, room_power_levels))),
                Some(LatestEventValue::KnockedStateEvent(_)),
                "can accept, cannot decline",
            );
        }

        // Cannot accept. Can decline.
        {
            let mut room_power_levels = room_power_levels.clone();
            room_power_levels.invite = 10.into();
            room_power_levels.kick = 0.into();
            assert_matches!(
                find_and_map(&event, &Some((user_id, room_power_levels))),
                Some(LatestEventValue::KnockedStateEvent(_)),
                "cannot accept, can decline",
            );
        }

        // Can accept. Can decline.
        {
            room_power_levels.invite = 0.into();
            room_power_levels.kick = 0.into();
            assert_matches!(
                find_and_map(&event, &Some((user_id, room_power_levels))),
                Some(LatestEventValue::KnockedStateEvent(_)),
                "can accept, can decline",
            );
        }
    }

    #[test]
    fn test_latest_event_value_room_message_verification_request() {
        assert_latest_event_value!(
            with |event_factory| {
                event_factory
                    .event(
                        ruma::events::room::message::RoomMessageEventContent::new(
                            ruma::events::room::message::MessageType::VerificationRequest(
                                ruma::events::room::message::KeyVerificationRequestEventContent::new(
                                    "body".to_owned(),
                                    vec![],
                                    ruma::OwnedDeviceId::from("device_id"),
                                    user_id!("@user:server.name").to_owned(),
                                )
                            )
                        )
                    )
                    .into_event()
            }
            it produces None
        );
    }
}

#[cfg(all(not(target_family = "wasm"), test))]
mod tests_non_wasm {
    use assert_matches::assert_matches;
    use matrix_sdk_test::{async_test, event_factory::EventFactory};
    use ruma::{event_id, room_id, user_id};

    use super::LatestEventValue;
    use crate::test_utils::mocks::MatrixMockServer;

    #[async_test]
    async fn test_latest_event_value_is_scanning_event_backwards_from_event_cache() {
        use matrix_sdk_base::{
            linked_chunk::{ChunkIdentifier, Position, Update},
            RoomState,
        };

        use crate::{client::WeakClient, room::WeakRoom};

        let room_id = room_id!("!r0");
        let user_id = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(user_id).room(room_id);
        let event_id_0 = event_id!("$ev0");
        let event_id_1 = event_id!("$ev1");
        let event_id_2 = event_id!("$ev2");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        // Prelude.
        {
            // Create the room.
            client.base_client().get_or_create_room(room_id, RoomState::Joined);

            // Initialise the event cache store.
            client
                .event_cache_store()
                .lock()
                .await
                .unwrap()
                .handle_linked_chunk_updates(
                    matrix_sdk_base::linked_chunk::LinkedChunkId::Room(room_id),
                    vec![
                        Update::NewItemsChunk {
                            previous: None,
                            new: ChunkIdentifier::new(0),
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(0), 0),
                            items: vec![
                                // a latest event candidate
                                event_factory.text_msg("hello").event_id(event_id_0).into(),
                                // a latest event candidate
                                event_factory.text_msg("world").event_id(event_id_1).into(),
                                // not a latest event candidate
                                event_factory
                                    .room_topic("new room topic")
                                    .event_id(event_id_2)
                                    .into(),
                            ],
                        },
                    ],
                )
                .await
                .unwrap();
        }

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let (room_event_cache, _) = event_cache.for_room(room_id).await.unwrap();
        let weak_room = WeakRoom::new(WeakClient::from_client(&client), room_id.to_owned());

        assert_matches!(
            LatestEventValue::new(room_id, None, &room_event_cache, &weak_room).await,
            LatestEventValue::RoomMessage(given_event) => {
                // We get `event_id_1` because `event_id_2` isn't a candidate,
                // and `event_id_0` hasn't been read yet (because events are
                // read backwards).
                assert_eq!(given_event.event_id(), event_id_1);
            }
        );
    }
}
