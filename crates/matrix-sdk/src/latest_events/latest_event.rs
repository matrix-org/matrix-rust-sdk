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

        if let LatestEventValue::None = new_value {
            // The new value is `None`. It means no new value has been
            // computed. Let's keep the old value.
        } else {
            self.value.set(new_value).await;
        }
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

pub fn find_and_map(
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
