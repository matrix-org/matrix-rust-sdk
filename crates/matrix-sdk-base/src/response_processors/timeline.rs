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

use matrix_sdk_common::deserialized_responses::TimelineEvent;
#[cfg(feature = "e2e-encryption")]
use ruma::events::SyncMessageLikeEvent;
use ruma::{
    events::{
        room::power_levels::{
            RoomPowerLevelsEvent, RoomPowerLevelsEventContent, StrippedRoomPowerLevelsEvent,
        },
        AnyStrippedStateEvent, AnySyncMessageLikeEvent, AnySyncStateEvent, AnySyncTimelineEvent,
        StateEventType,
    },
    push::{Action, PushConditionRoomCtx},
    RoomVersionId, UInt, UserId,
};
use tracing::{instrument, trace, warn};

use super::Context;
#[cfg(feature = "e2e-encryption")]
use super::{e2ee, verification};
use crate::{
    deserialized_responses::RawAnySyncOrStrippedTimelineEvent,
    store::{BaseStateStore, StateStoreExt as _},
    sync::{Notification, Timeline},
    Result, Room, RoomInfo,
};

/// Process a set of sync timeline event, and create a [`Timeline`].
///
/// For each event:
/// - will try to decrypt it,
/// - will process verification,
/// - will process redaction,
/// - will process notification.
#[instrument(skip_all, fields(room_id = ?room_info.room_id))]
pub async fn build<'notification, 'e2ee>(
    context: &mut Context,
    room: &Room,
    room_info: &mut RoomInfo,
    timeline_inputs: builder::Timeline,
    notification_inputs: builder::Notification<'notification>,
    #[cfg(feature = "e2e-encryption")] e2ee: builder::E2EE<'e2ee>,
) -> Result<Timeline> {
    let mut timeline = Timeline::new(timeline_inputs.limited, timeline_inputs.prev_batch);
    let mut push_context =
        get_push_room_context(context, room, room_info, notification_inputs.state_store).await?;
    let room_id = room.room_id();

    for raw_event in timeline_inputs.raw_events {
        // Start by assuming we have a plaintext event. We'll replace it with a
        // decrypted or UTD event below if necessary.
        let mut timeline_event = TimelineEvent::new(raw_event);

        // Do some special stuff on the `timeline_event` before collecting it.
        match timeline_event.raw().deserialize() {
            Ok(sync_timeline_event) => {
                #[allow(clippy::single_match)]
                match &sync_timeline_event {
                    // State events are ignored. They must be processed separately.
                    AnySyncTimelineEvent::State(_) => {
                        // do nothing
                    }

                    // A room redaction.
                    AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomRedaction(
                        redaction_event,
                    )) => {
                        let room_version = room_info.room_version().unwrap_or(&RoomVersionId::V1);

                        if let Some(redacts) = redaction_event.redacts(room_version) {
                            room_info
                                .handle_redaction(redaction_event, timeline_event.raw().cast_ref());

                            context.state_changes.add_redaction(
                                room_id,
                                redacts,
                                timeline_event.raw().clone().cast(),
                            );
                        }
                    }

                    // Decrypt encrypted event, or process verification event.
                    #[cfg(feature = "e2e-encryption")]
                    AnySyncTimelineEvent::MessageLike(sync_message_like_event) => {
                        match sync_message_like_event {
                            AnySyncMessageLikeEvent::RoomEncrypted(
                                SyncMessageLikeEvent::Original(_),
                            ) => {
                                if let Some(decrypted_timeline_event) =
                                    Box::pin(e2ee::decrypt::sync_timeline_event(
                                        context,
                                        e2ee.olm_machine,
                                        timeline_event.raw(),
                                        room_id,
                                        e2ee.decryption_trust_requirement,
                                        e2ee.verification_is_allowed,
                                    ))
                                    .await?
                                {
                                    timeline_event = decrypted_timeline_event;
                                }
                            }

                            _ => {
                                Box::pin(verification::process_if_candidate(
                                    context,
                                    &sync_timeline_event,
                                    e2ee.verification_is_allowed,
                                    e2ee.olm_machine,
                                    room_id,
                                ))
                                .await?;
                            }
                        }
                    }

                    // Nothing particular to do.
                    #[cfg(not(feature = "e2e-encryption"))]
                    AnySyncTimelineEvent::MessageLike(_) => (),
                }

                if let Some(push_context) = &mut push_context {
                    update_push_room_context(context, push_context, room.own_user_id(), room_info)
                } else {
                    push_context = get_push_room_context(
                        context,
                        room,
                        room_info,
                        notification_inputs.state_store,
                    )
                    .await?;
                }

                if let Some(context) = &push_context {
                    let actions =
                        notification_inputs.push_rules.get_actions(timeline_event.raw(), context);

                    if actions.iter().any(Action::should_notify) {
                        notification_inputs
                            .notifications
                            .entry(room_id.to_owned())
                            .or_default()
                            .push(Notification {
                                actions: actions.to_owned(),
                                event: RawAnySyncOrStrippedTimelineEvent::Sync(
                                    timeline_event.raw().clone(),
                                ),
                            });
                    }

                    timeline_event.push_actions = Some(actions.to_owned());
                }
            }
            Err(error) => {
                warn!("Error deserializing event: {error}");
            }
        }

        // Finally, we have process the timeline event. We can collect it.
        timeline.events.push(timeline_event);
    }

    Ok(timeline)
}

/// Set of types used by [`build`] to reduce the number of arguments by grouping
/// them by thematics.
pub mod builder {
    use std::collections::BTreeMap;

    #[cfg(feature = "e2e-encryption")]
    use matrix_sdk_crypto::{OlmMachine, TrustRequirement};
    use ruma::{
        api::client::sync::sync_events::{v3, v5},
        events::AnySyncTimelineEvent,
        push::Ruleset,
        serde::Raw,
        OwnedRoomId,
    };

    use crate::{store::BaseStateStore, sync};

    pub struct Timeline {
        pub limited: bool,
        pub raw_events: Vec<Raw<AnySyncTimelineEvent>>,
        pub prev_batch: Option<String>,
    }

    impl From<v3::Timeline> for Timeline {
        fn from(value: v3::Timeline) -> Self {
            Self { limited: value.limited, raw_events: value.events, prev_batch: value.prev_batch }
        }
    }

    impl From<&v5::response::Room> for Timeline {
        fn from(value: &v5::response::Room) -> Self {
            Self {
                limited: value.limited,
                raw_events: value.timeline.clone(),
                prev_batch: value.prev_batch.clone(),
            }
        }
    }

    pub struct Notification<'a> {
        pub push_rules: &'a Ruleset,
        pub notifications: &'a mut BTreeMap<OwnedRoomId, Vec<sync::Notification>>,
        pub state_store: &'a BaseStateStore,
    }

    impl<'a> Notification<'a> {
        pub fn new(
            push_rules: &'a Ruleset,
            notifications: &'a mut BTreeMap<OwnedRoomId, Vec<sync::Notification>>,
            state_store: &'a BaseStateStore,
        ) -> Self {
            Self { push_rules, notifications, state_store }
        }
    }

    #[cfg(feature = "e2e-encryption")]
    pub struct E2EE<'a> {
        pub olm_machine: Option<&'a OlmMachine>,
        pub decryption_trust_requirement: TrustRequirement,
        pub verification_is_allowed: bool,
    }

    #[cfg(feature = "e2e-encryption")]
    impl<'a> E2EE<'a> {
        pub fn new(
            olm_machine: Option<&'a OlmMachine>,
            decryption_trust_requirement: TrustRequirement,
            verification_is_allowed: bool,
        ) -> Self {
            Self { olm_machine, decryption_trust_requirement, verification_is_allowed }
        }
    }
}

/// Update the push context for the given room.
///
/// Updates the context data from `context.state_changes` or `room_info`.
fn update_push_room_context(
    context: &mut Context,
    push_rules: &mut PushConditionRoomCtx,
    user_id: &UserId,
    room_info: &RoomInfo,
) {
    let room_id = &*room_info.room_id;

    push_rules.member_count = UInt::new(room_info.active_members_count()).unwrap_or(UInt::MAX);

    // TODO: Use if let chain once stable
    if let Some(AnySyncStateEvent::RoomMember(member)) =
        context.state_changes.state.get(room_id).and_then(|events| {
            events.get(&StateEventType::RoomMember)?.get(user_id.as_str())?.deserialize().ok()
        })
    {
        push_rules.user_display_name = member
            .as_original()
            .and_then(|ev| ev.content.displayname.clone())
            .unwrap_or_else(|| user_id.localpart().to_owned())
    }

    if let Some(AnySyncStateEvent::RoomPowerLevels(event)) =
        context.state_changes.state.get(room_id).and_then(|types| {
            types.get(&StateEventType::RoomPowerLevels)?.get("")?.deserialize().ok()
        })
    {
        push_rules.power_levels = Some(event.power_levels().into());
    }
}

/// Get the push context for the given room.
///
/// Tries to get the data from `changes` or the up to date `room_info`.
/// Loads the data from the store otherwise.
///
/// Returns `None` if some data couldn't be found. This should only happen
/// in brand new rooms, while we process its state.
pub async fn get_push_room_context(
    context: &mut Context,
    room: &Room,
    room_info: &RoomInfo,
    state_store: &BaseStateStore,
) -> Result<Option<PushConditionRoomCtx>> {
    let room_id = room.room_id();
    let user_id = room.own_user_id();

    let member_count = room_info.active_members_count();

    // TODO: Use if let chain once stable
    let user_display_name = if let Some(AnySyncStateEvent::RoomMember(member)) =
        context.state_changes.state.get(room_id).and_then(|events| {
            events.get(&StateEventType::RoomMember)?.get(user_id.as_str())?.deserialize().ok()
        }) {
        member
            .as_original()
            .and_then(|ev| ev.content.displayname.clone())
            .unwrap_or_else(|| user_id.localpart().to_owned())
    } else if let Some(AnyStrippedStateEvent::RoomMember(member)) =
        context.state_changes.stripped_state.get(room_id).and_then(|events| {
            events.get(&StateEventType::RoomMember)?.get(user_id.as_str())?.deserialize().ok()
        })
    {
        member.content.displayname.unwrap_or_else(|| user_id.localpart().to_owned())
    } else if let Some(member) = Box::pin(room.get_member(user_id)).await? {
        member.name().to_owned()
    } else {
        trace!("Couldn't get push context because of missing own member information");
        return Ok(None);
    };

    let power_levels = if let Some(event) =
        context.state_changes.state.get(room_id).and_then(|types| {
            types
                .get(&StateEventType::RoomPowerLevels)?
                .get("")?
                .deserialize_as::<RoomPowerLevelsEvent>()
                .ok()
        }) {
        Some(event.power_levels().into())
    } else if let Some(event) =
        context.state_changes.stripped_state.get(room_id).and_then(|types| {
            types
                .get(&StateEventType::RoomPowerLevels)?
                .get("")?
                .deserialize_as::<StrippedRoomPowerLevelsEvent>()
                .ok()
        })
    {
        Some(event.power_levels().into())
    } else {
        state_store
            .get_state_event_static::<RoomPowerLevelsEventContent>(room_id)
            .await?
            .and_then(|e| e.deserialize().ok())
            .map(|event| event.power_levels().into())
    };

    Ok(Some(PushConditionRoomCtx {
        user_id: user_id.to_owned(),
        room_id: room_id.to_owned(),
        member_count: UInt::new(member_count).unwrap_or(UInt::MAX),
        user_display_name,
        power_levels,
    }))
}
