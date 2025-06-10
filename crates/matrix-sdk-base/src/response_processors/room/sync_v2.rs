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

use std::collections::{BTreeMap, BTreeSet};

use ruma::{
    api::client::sync::sync_events::v3::{InvitedRoom, JoinedRoom, KnockedRoom, LeftRoom},
    OwnedRoomId, OwnedUserId, RoomId,
};
use tokio::sync::broadcast::Sender;

#[cfg(feature = "e2e-encryption")]
use super::super::e2ee;
use super::{
    super::{account_data, ephemeral_events, notification, state_events, timeline, Context},
    RoomCreationData,
};
use crate::{
    sync::{InvitedRoomUpdate, JoinedRoomUpdate, KnockedRoomUpdate, LeftRoomUpdate},
    Result, RoomInfoNotableUpdate, RoomState,
};

/// Process updates of a joined room.
#[allow(clippy::too_many_arguments)]
pub async fn update_joined_room(
    context: &mut Context,
    room_creation_data: RoomCreationData<'_>,
    joined_room: JoinedRoom,
    updated_members_in_room: &mut BTreeMap<OwnedRoomId, BTreeSet<OwnedUserId>>,
    notification: notification::Notification<'_>,
    #[cfg(feature = "e2e-encryption")] e2ee: e2ee::E2EE<'_>,
) -> Result<JoinedRoomUpdate> {
    let RoomCreationData {
        room_id,
        room_info_notable_update_sender,
        requested_required_states,
        ambiguity_cache,
    } = room_creation_data;

    let state_store = notification.state_store;

    let room =
        state_store.get_or_create_room(room_id, RoomState::Joined, room_info_notable_update_sender);

    let mut room_info = room.clone_info();

    room_info.mark_as_joined();
    room_info.update_from_ruma_summary(&joined_room.summary);
    room_info.set_prev_batch(joined_room.timeline.prev_batch.as_deref());
    room_info.mark_state_fully_synced();
    room_info.handle_encryption_state(requested_required_states.for_room(room_id));

    let (raw_state_events, state_events) = state_events::sync::collect(&joined_room.state.events);

    let mut new_user_ids = BTreeSet::new();

    state_events::sync::dispatch(
        context,
        (&raw_state_events, &state_events),
        &mut room_info,
        ambiguity_cache,
        &mut new_user_ids,
        state_store,
    )
    .await?;

    ephemeral_events::dispatch(context, &joined_room.ephemeral.events, room_id);

    if joined_room.timeline.limited {
        room_info.mark_members_missing();
    }

    let (raw_state_events_from_timeline, state_events_from_timeline) =
        state_events::sync::collect_from_timeline(&joined_room.timeline.events);

    state_events::sync::dispatch(
        context,
        (&raw_state_events_from_timeline, &state_events_from_timeline),
        &mut room_info,
        ambiguity_cache,
        &mut new_user_ids,
        state_store,
    )
    .await?;

    #[cfg(feature = "e2e-encryption")]
    let olm_machine = e2ee.olm_machine;

    let timeline = timeline::build(
        context,
        &room,
        &mut room_info,
        timeline::builder::Timeline::from(joined_room.timeline),
        notification,
        #[cfg(feature = "e2e-encryption")]
        e2ee,
    )
    .await?;

    // Save the new `RoomInfo`.
    context.state_changes.add_room(room_info);

    account_data::for_room(context, room_id, &joined_room.account_data.events, state_store).await;

    // `processors::account_data::from_room` might have updated the `RoomInfo`.
    // Let's fetch it again.
    //
    // SAFETY: `expect` is safe because the `RoomInfo` has been inserted 2 lines
    // above.
    let mut room_info = context
        .state_changes
        .room_infos
        .get(room_id)
        .expect("`RoomInfo` must exist in `StateChanges` at this point")
        .clone();

    #[cfg(feature = "e2e-encryption")]
    e2ee::tracked_users::update_or_set_if_room_is_newly_encrypted(
        olm_machine,
        &new_user_ids,
        room_info.encryption_state(),
        room.encryption_state(),
        room_id,
        state_store,
    )
    .await?;

    updated_members_in_room.insert(room_id.to_owned(), new_user_ids);

    let notification_count = joined_room.unread_notifications.into();
    room_info.update_notification_count(notification_count);

    context.state_changes.add_room(room_info);

    Ok(JoinedRoomUpdate::new(
        timeline,
        joined_room.state.events,
        joined_room.account_data.events,
        joined_room.ephemeral.events,
        notification_count,
        ambiguity_cache.changes.remove(room_id).unwrap_or_default(),
    ))
}

/// Process historical updates of a left room.
#[allow(clippy::too_many_arguments)]
pub async fn update_left_room(
    context: &mut Context,
    room_creation_data: RoomCreationData<'_>,
    left_room: LeftRoom,
    notification: notification::Notification<'_>,
    #[cfg(feature = "e2e-encryption")] e2ee: e2ee::E2EE<'_>,
) -> Result<LeftRoomUpdate> {
    let RoomCreationData {
        room_id,
        room_info_notable_update_sender,
        requested_required_states,
        ambiguity_cache,
    } = room_creation_data;

    let state_store = notification.state_store;

    let room =
        state_store.get_or_create_room(room_id, RoomState::Left, room_info_notable_update_sender);

    let mut room_info = room.clone_info();
    room_info.mark_as_left();
    room_info.mark_state_partially_synced();
    room_info.handle_encryption_state(requested_required_states.for_room(room_id));

    let (raw_state_events, state_events) = state_events::sync::collect(&left_room.state.events);

    state_events::sync::dispatch(
        context,
        (&raw_state_events, &state_events),
        &mut room_info,
        ambiguity_cache,
        &mut (),
        state_store,
    )
    .await?;

    let (raw_state_events_from_timeline, state_events_from_timeline) =
        state_events::sync::collect_from_timeline(&left_room.timeline.events);

    state_events::sync::dispatch(
        context,
        (&raw_state_events_from_timeline, &state_events_from_timeline),
        &mut room_info,
        ambiguity_cache,
        &mut (),
        state_store,
    )
    .await?;

    let timeline = timeline::build(
        context,
        &room,
        &mut room_info,
        timeline::builder::Timeline::from(left_room.timeline),
        notification,
        #[cfg(feature = "e2e-encryption")]
        e2ee,
    )
    .await?;

    // Save the new `RoomInfo`.
    context.state_changes.add_room(room_info);

    account_data::for_room(context, room_id, &left_room.account_data.events, state_store).await;

    let ambiguity_changes = ambiguity_cache.changes.remove(room_id).unwrap_or_default();

    Ok(LeftRoomUpdate::new(
        timeline,
        left_room.state.events,
        left_room.account_data.events,
        ambiguity_changes,
    ))
}

/// Process updates of an invited room.
pub async fn update_invited_room(
    context: &mut Context,
    room_id: &RoomId,
    invited_room: InvitedRoom,
    room_info_notable_update_sender: Sender<RoomInfoNotableUpdate>,
    notification: notification::Notification<'_>,
) -> Result<InvitedRoomUpdate> {
    let state_store = notification.state_store;

    let room = state_store.get_or_create_room(
        room_id,
        RoomState::Invited,
        room_info_notable_update_sender,
    );

    let (raw_events, events) = state_events::stripped::collect(&invited_room.invite_state.events);

    let mut room_info = room.clone_info();
    room_info.mark_as_invited();
    room_info.mark_state_fully_synced();

    state_events::stripped::dispatch_invite_or_knock(
        context,
        (&raw_events, &events),
        &room,
        &mut room_info,
        notification,
    )
    .await?;

    context.state_changes.add_room(room_info);

    Ok(invited_room)
}

/// Process updates of a knocked room.
pub async fn update_knocked_room(
    context: &mut Context,
    room_id: &RoomId,
    knocked_room: KnockedRoom,
    room_info_notable_update_sender: Sender<RoomInfoNotableUpdate>,
    notification: notification::Notification<'_>,
) -> Result<KnockedRoomUpdate> {
    let state_store = notification.state_store;

    let room = state_store.get_or_create_room(
        room_id,
        RoomState::Knocked,
        room_info_notable_update_sender,
    );

    let (raw_events, events) = state_events::stripped::collect(&knocked_room.knock_state.events);

    let mut room_info = room.clone_info();
    room_info.mark_as_knocked();
    room_info.mark_state_fully_synced();

    state_events::stripped::dispatch_invite_or_knock(
        context,
        (&raw_events, &events),
        &room,
        &mut room_info,
        notification,
    )
    .await?;

    context.state_changes.add_room(room_info);

    Ok(knocked_room)
}
