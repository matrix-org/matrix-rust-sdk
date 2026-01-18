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

pub mod extensions;

use std::collections::BTreeMap;
#[cfg(feature = "e2e-encryption")]
use std::collections::BTreeSet;

use as_variant::as_variant;
use matrix_sdk_common::timer;
use ruma::{
    JsOption, OwnedRoomId, RoomId, UserId,
    api::client::sync::sync_events::{
        v3::{InviteState, InvitedRoom, KnockState, KnockedRoom},
        v5 as http,
    },
    assign,
    events::{
        AnyRoomAccountDataEvent, AnyStrippedStateEvent, AnySyncStateEvent, StateEventType,
        room::member::{MembershipState, RoomMemberEventContent},
    },
    serde::Raw,
};
use tokio::sync::broadcast::Sender;

#[cfg(feature = "e2e-encryption")]
use super::super::e2ee;
use super::{
    super::{Context, notification, state_events, timeline},
    RoomCreationData,
};
use crate::{
    Result, Room, RoomHero, RoomInfo, RoomInfoNotableUpdate, RoomInfoNotableUpdateReasons,
    RoomState,
    store::BaseStateStore,
    sync::{InvitedRoomUpdate, JoinedRoomUpdate, KnockedRoomUpdate, LeftRoomUpdate, State},
    utils::RawSyncStateEventWithKeys,
};

/// Represent any kind of room updates.
pub enum RoomUpdateKind {
    Joined(JoinedRoomUpdate),
    Left(LeftRoomUpdate),
    Invited(InvitedRoomUpdate),
    Knocked(KnockedRoomUpdate),
}

pub async fn update_any_room(
    context: &mut Context,
    user_id: &UserId,
    room_creation_data: RoomCreationData<'_>,
    room_response: &http::response::Room,
    rooms_account_data: &BTreeMap<OwnedRoomId, Vec<Raw<AnyRoomAccountDataEvent>>>,
    #[cfg(feature = "e2e-encryption")] e2ee: e2ee::E2EE<'_>,
    notification: notification::Notification<'_>,
) -> Result<Option<(RoomInfo, RoomUpdateKind)>> {
    let _timer = timer!(tracing::Level::TRACE, "update_any_room");

    let RoomCreationData {
        room_id,
        room_info_notable_update_sender,
        requested_required_states,
        ambiguity_cache,
    } = room_creation_data;

    // Read state events from the `required_state` field.
    //
    // Don't read state events from the `timeline` field, because they might be
    // incomplete or staled already. We must only read state events from
    // `required_state`.
    let state = State::from_msc4186(room_response.required_state.clone());
    let mut raw_state_events = state.collect(&[]);

    let state_store = notification.state_store;

    // Find or create the room in the store
    let is_new_room = !state_store.room_exists(room_id);

    let invite_state_events =
        room_response.invite_state.as_ref().map(|events| state_events::stripped::collect(events));

    #[allow(unused_mut)] // Required for some feature flag combinations
    let (mut room, mut room_info, maybe_room_update_kind) = membership(
        context,
        &mut raw_state_events,
        &invite_state_events,
        state_store,
        user_id,
        room_id,
        room_info_notable_update_sender,
    );

    room_info.mark_state_partially_synced();
    room_info.handle_encryption_state(requested_required_states.for_room(room_id));

    #[cfg(feature = "e2e-encryption")]
    let mut new_user_ids = BTreeSet::new();

    #[cfg(not(feature = "e2e-encryption"))]
    let mut new_user_ids = ();

    state_events::sync::dispatch(
        context,
        raw_state_events,
        &mut room_info,
        ambiguity_cache,
        &mut new_user_ids,
        state_store,
        #[cfg(feature = "experimental-encrypted-state-events")]
        e2ee.clone(),
    )
    .await?;

    // This will be used for both invited and knocked rooms.
    if let Some((raw_events, events)) = invite_state_events {
        state_events::stripped::dispatch_invite_or_knock(
            context,
            (&raw_events, &events),
            &room,
            &mut room_info,
            notification::Notification::new(
                notification.push_rules,
                notification.notifications,
                notification.state_store,
            ),
        )
        .await?;
    }

    properties(context, room_id, room_response, &mut room_info, is_new_room);

    let timeline = timeline::build(
        context,
        &room,
        &mut room_info,
        timeline::builder::Timeline::from(room_response),
        notification,
        #[cfg(feature = "e2e-encryption")]
        e2ee.clone(),
    )
    .await?;

    #[cfg(feature = "e2e-encryption")]
    e2ee::tracked_users::update_or_set_if_room_is_newly_encrypted(
        e2ee.olm_machine,
        &new_user_ids,
        room_info.encryption_state(),
        room.encryption_state(),
        room_id,
        state_store,
    )
    .await?;

    let notification_count = room_response.unread_notifications.clone().into();
    room_info.update_notification_count(notification_count);

    let ambiguity_changes = ambiguity_cache.changes.remove(room_id).unwrap_or_default();
    let room_account_data = rooms_account_data.get(room_id);

    match (room_info.state(), maybe_room_update_kind) {
        (RoomState::Joined, None) => {
            // Ephemeral events are added separately, because we might not
            // have a room subsection in the response, yet we may have receipts for
            // that room.
            let ephemeral = Vec::new();

            Ok(Some((
                room_info,
                RoomUpdateKind::Joined(JoinedRoomUpdate::new(
                    timeline,
                    state,
                    room_account_data.cloned().unwrap_or_default(),
                    ephemeral,
                    notification_count,
                    ambiguity_changes,
                )),
            )))
        }

        (RoomState::Left, None) | (RoomState::Banned, None) => Ok(Some((
            room_info,
            RoomUpdateKind::Left(LeftRoomUpdate::new(
                timeline,
                state,
                room_account_data.cloned().unwrap_or_default(),
                ambiguity_changes,
            )),
        ))),

        (RoomState::Invited, Some(update @ RoomUpdateKind::Invited(_)))
        | (RoomState::Knocked, Some(update @ RoomUpdateKind::Knocked(_))) => {
            Ok(Some((room_info, update)))
        }

        _ => Ok(None),
    }
}

/// Look through the sliding sync data for this room, find/create it in the
/// store, and process any invite information.
///
/// If there is any invite state events, the room can be considered an invited
/// or knocked room, depending of the membership event (if any).
fn membership(
    context: &mut Context,
    state_events: &mut [RawSyncStateEventWithKeys],
    invite_state_events: &Option<(Vec<Raw<AnyStrippedStateEvent>>, Vec<AnyStrippedStateEvent>)>,
    store: &BaseStateStore,
    user_id: &UserId,
    room_id: &RoomId,
    room_info_notable_update_sender: Sender<RoomInfoNotableUpdate>,
) -> (Room, RoomInfo, Option<RoomUpdateKind>) {
    // There are invite state events. It means the room can be:
    //
    // 1. either an invited room,
    // 2. or a knocked room.
    //
    // Let's find out.
    if let Some(state_events) = invite_state_events {
        // We need to find the membership event since it could be for either an invited
        // or knocked room.
        let membership_event = state_events.1.iter().find_map(|event| {
            if let AnyStrippedStateEvent::RoomMember(membership_event) = event
                && membership_event.state_key == user_id
            {
                return Some(membership_event.content.clone());
            }
            None
        });

        match membership_event {
            // There is a membership event indicating it's a knocked room.
            Some(RoomMemberEventContent { membership: MembershipState::Knock, .. }) => {
                let room = store.get_or_create_room(
                    room_id,
                    RoomState::Knocked,
                    room_info_notable_update_sender,
                );
                let mut room_info = room.clone_info();
                // Override the room state if the room already exists.
                room_info.mark_as_knocked();

                let raw_events = state_events.0.clone();
                let knock_state = assign!(KnockState::default(), { events: raw_events });
                let knocked_room = assign!(KnockedRoom::default(), { knock_state: knock_state });

                (room, room_info, Some(RoomUpdateKind::Knocked(knocked_room)))
            }

            // Otherwise, assume it's an invited room because there are invite state events.
            _ => {
                let room = store.get_or_create_room(
                    room_id,
                    RoomState::Invited,
                    room_info_notable_update_sender,
                );
                let mut room_info = room.clone_info();
                // Override the room state if the room already exists.
                room_info.mark_as_invited();

                let raw_events = state_events.0.clone();
                let invited_room = InvitedRoom::from(InviteState::from(raw_events));

                (room, room_info, Some(RoomUpdateKind::Invited(invited_room)))
            }
        }
    }
    // No invite state events. We assume this is a joined room for the moment. See this block to
    // learn more.
    else {
        let room =
            store.get_or_create_room(room_id, RoomState::Joined, room_info_notable_update_sender);
        let mut room_info = room.clone_info();

        // We default to considering this room joined if it's not an invite. If it's
        // actually left (and we remembered to request membership events in
        // our sync request), then we can find this out from the events in
        // required_state by calling handle_own_room_membership.
        room_info.mark_as_joined();

        // We don't need to do this in a v2 sync, because the membership of a room can
        // be figured out by whether the room is in the "join", "leave" etc.
        // property. In sliding sync we only have invite_state,
        // required_state and timeline, so we must process required_state and timeline
        // looking for relevant membership events.
        own_membership(context, user_id, state_events, &mut room_info);

        (room, room_info, None)
    }
}

/// Find any `m.room.member` events that refer to the current user, and update
/// the state in room_info to reflect the "membership" property.
fn own_membership(
    context: &mut Context,
    user_id: &UserId,
    state_events: &mut [RawSyncStateEventWithKeys],
    room_info: &mut RoomInfo,
) {
    // Start from the last event; the first membership event we see in that order is
    // the last in the regular order, so that's the only one we need to
    // consider.
    for event in state_events.iter_mut().rev() {
        // If this event updates the current user's membership, record that in the
        // room_info.
        if event.event_type == StateEventType::RoomMember
            && event.state_key.as_str() == user_id
            && let Some(member) = event
                .deserialize_as(|any_event| as_variant!(any_event, AnySyncStateEvent::RoomMember))
        {
            let new_state: RoomState = member.membership().into();

            if new_state != room_info.state() {
                room_info.set_state(new_state);
                // Update an existing notable update entry or create a new one
                context
                    .room_info_notable_updates
                    .entry(room_info.room_id.to_owned())
                    .or_default()
                    .insert(RoomInfoNotableUpdateReasons::MEMBERSHIP);
            }

            break;
        }
    }
}

fn properties(
    context: &mut Context,
    room_id: &RoomId,
    room_response: &http::response::Room,
    room_info: &mut RoomInfo,
    is_new_room: bool,
) {
    // Handle the room's avatar.
    //
    // It can be updated via the state events, or via the
    // [`http::ResponseRoom::avatar`] field. This part of the code handles the
    // latter case. The former case is handled by [`BaseClient::handle_state`].
    match &room_response.avatar {
        // A new avatar!
        JsOption::Some(avatar_uri) => room_info.update_avatar(Some(avatar_uri.to_owned())),
        // Avatar must be removed.
        JsOption::Null => room_info.update_avatar(None),
        // Nothing to do.
        JsOption::Undefined => {}
    }

    // Sliding sync doesn't have a room summary, nevertheless it contains the joined
    // and invited member counts, in addition to the heroes.
    if let Some(count) = room_response.joined_count {
        room_info.update_joined_member_count(count.into());
    }
    if let Some(count) = room_response.invited_count {
        room_info.update_invited_member_count(count.into());
    }

    if let Some(heroes) = &room_response.heroes {
        room_info.update_heroes(
            heroes
                .iter()
                .map(|hero| RoomHero {
                    user_id: hero.user_id.clone(),
                    display_name: hero.name.clone(),
                    avatar_url: hero.avatar.clone(),
                })
                .collect(),
        );
    }

    room_info.set_prev_batch(room_response.prev_batch.as_deref());

    if room_response.limited {
        room_info.mark_members_missing();
    }

    if let Some(recency_stamp) = &room_response.bump_stamp {
        let recency_stamp = u64::from(*recency_stamp).into();

        if room_info.recency_stamp.as_ref() != Some(&recency_stamp) {
            room_info.update_recency_stamp(recency_stamp);

            // If it's not a new room, let's emit a `RECENCY_STAMP` update.
            // For a new room, the room will appear as new, so we don't care about this
            // update.
            if !is_new_room {
                context
                    .room_info_notable_updates
                    .entry(room_id.to_owned())
                    .or_default()
                    .insert(RoomInfoNotableUpdateReasons::RECENCY_STAMP);
            }
        }
    }
}

impl State {
    /// Construct a [`State`] from the state changes for a joined or left room
    /// from a response of the Simplified Sliding Sync endpoint.
    fn from_msc4186(events: Vec<Raw<AnySyncStateEvent>>) -> Self {
        Self::After(events)
    }
}
