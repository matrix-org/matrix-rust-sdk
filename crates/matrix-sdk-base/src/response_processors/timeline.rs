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

use matrix_sdk_common::{deserialized_responses::TimelineEvent, timer};
#[cfg(feature = "e2e-encryption")]
use ruma::events::SyncMessageLikeEvent;
use ruma::{
    MilliSecondsSinceUnixEpoch, UInt, UserId, assign,
    events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent},
    push::{Action, PushConditionRoomCtx},
};
use tracing::{instrument, trace, warn};

use super::{Context, notification};
#[cfg(feature = "e2e-encryption")]
use super::{e2ee, verification};
use crate::{Result, Room, RoomInfo, sync::Timeline};

/// Process a set of sync timeline event, and create a [`Timeline`].
///
/// For each event:
/// - will try to decrypt it,
/// - will fix the `origin_server_ts` if considered invalid,
/// - will process verification,
/// - will process redaction,
/// - will process notification.
#[allow(clippy::extra_unused_lifetimes)]
#[instrument(skip_all, fields(room_id = ?room_info.room_id))]
pub async fn build<'notification, 'e2ee>(
    context: &mut Context,
    room: &Room,
    room_info: &mut RoomInfo,
    timeline_inputs: builder::Timeline,
    mut notification: notification::Notification<'notification>,
    #[cfg(feature = "e2e-encryption")] e2ee: e2ee::E2EE<'e2ee>,
) -> Result<Timeline> {
    let _timer = timer!(tracing::Level::TRACE, "build a timeline from sync");

    let now = MilliSecondsSinceUnixEpoch::now();
    let mut timeline = Timeline::new(timeline_inputs.limited, timeline_inputs.prev_batch);
    let mut push_condition_room_ctx = get_push_room_context(context, room, room_info).await?;
    let room_id = room.room_id();

    for raw_event in timeline_inputs.raw_events {
        // Start by assuming we have a plaintext event. We'll replace it with a
        // decrypted or UTD event below if necessary.
        let mut timeline_event = TimelineEvent::from_plaintext_with_max_timestamp(raw_event, now);

        // Do some special stuff on the `timeline_event` before collecting it.
        match timeline_event.raw().deserialize() {
            Ok(sync_timeline_event) => {
                match &sync_timeline_event {
                    // State events are ignored. They must be processed separately.
                    AnySyncTimelineEvent::State(_) => {
                        // do nothing
                    }

                    // A room redaction.
                    AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomRedaction(
                        redaction_event,
                    )) => {
                        let redaction_rules = room_info.room_version_rules_or_default().redaction;

                        if let Some(redacts) = redaction_event.redacts(&redaction_rules) {
                            room_info.handle_redaction(
                                redaction_event,
                                timeline_event.raw().cast_ref_unchecked(),
                            );

                            context.state_changes.add_redaction(
                                room_id,
                                redacts,
                                timeline_event.raw().clone().cast_unchecked(),
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
                                        e2ee.clone(),
                                        &timeline_event,
                                        room_id,
                                    ))
                                    .await?
                                {
                                    timeline_event = decrypted_timeline_event;
                                }
                            }

                            _ => {
                                Box::pin(verification::process_if_relevant(
                                    &sync_timeline_event,
                                    e2ee.clone(),
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

                if let Some(push_condition_room_ctx) = &mut push_condition_room_ctx {
                    update_push_room_context(
                        context,
                        push_condition_room_ctx,
                        room.own_user_id(),
                        room_info,
                    )
                } else {
                    push_condition_room_ctx =
                        get_push_room_context(context, room, room_info).await?;
                }

                if let Some(push_condition_room_ctx) = &push_condition_room_ctx {
                    let actions = notification
                        .push_notification_from_event_if(
                            push_condition_room_ctx,
                            timeline_event.raw(),
                            Action::should_notify,
                        )
                        .await;

                    timeline_event.set_push_actions(actions.to_owned());
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
    use ruma::{
        api::client::sync::sync_events::{v3, v5},
        events::AnySyncTimelineEvent,
        serde::Raw,
    };

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
}

/// Update the push context for the given room.
///
/// Updates the context data from `context.state_changes` or `room_info`.
fn update_push_room_context(
    context: &Context,
    push_rules: &mut PushConditionRoomCtx,
    user_id: &UserId,
    room_info: &RoomInfo,
) {
    let room_id = &*room_info.room_id;

    push_rules.member_count = UInt::new(room_info.active_members_count()).unwrap_or(UInt::MAX);

    // TODO: Use if let chain once stable
    if let Some(member) = context.state_changes.member(room_id, user_id) {
        push_rules.user_display_name =
            member.content.displayname.unwrap_or_else(|| user_id.localpart().to_owned())
    }

    if let Some(power_levels) = context.state_changes.power_levels(room_id) {
        push_rules.power_levels = Some(power_levels.into());
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
    context: &Context,
    room: &Room,
    room_info: &RoomInfo,
) -> Result<Option<PushConditionRoomCtx>> {
    let room_id = room.room_id();
    let user_id = room.own_user_id();

    let member_count = room_info.active_members_count();

    // TODO: Use if let chain once stable
    let user_display_name = if let Some(member) = context.state_changes.member(room_id, user_id) {
        member.content.displayname.unwrap_or_else(|| user_id.localpart().to_owned())
    } else if let Some(member) = Box::pin(room.get_member(user_id)).await? {
        member.name().to_owned()
    } else {
        trace!("Couldn't get push context because of missing own member information");
        return Ok(None);
    };

    let power_levels = if let Some(power_levels) = context.state_changes.power_levels(room_id) {
        Some(power_levels)
    } else {
        room.power_levels().await.ok()
    };

    Ok(Some(assign!(
        PushConditionRoomCtx::new(
            room_id.to_owned(),
            UInt::new(member_count).unwrap_or(UInt::MAX),
            user_id.to_owned(),
            user_display_name
        ),
        { power_levels: power_levels.map(Into::into) }
    )))
}
