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

use std::collections::BTreeMap;
#[cfg(feature = "e2e-encryption")]
use std::ops::Deref;

use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
#[cfg(feature = "e2e-encryption")]
use ruma::events::AnyToDeviceEvent;
use ruma::{
    api::client::sync::sync_events::{
        v3::{self, InvitedRoom},
        v4,
    },
    events::{AnyRoomAccountDataEvent, AnySyncStateEvent, AnySyncTimelineEvent},
    serde::Raw,
    JsOption, OwnedRoomId, RoomId,
};
use tracing::{instrument, trace, warn};

use super::BaseClient;
#[cfg(feature = "e2e-encryption")]
use crate::latest_event::{is_suitable_for_latest_event, LatestEvent, PossibleLatestEvent};
#[cfg(feature = "e2e-encryption")]
use crate::RoomMemberships;
use crate::{
    error::Result,
    read_receipts::{compute_unread_counts, PreviousEventsProvider},
    rooms::RoomState,
    store::{ambiguity_map::AmbiguityCache, StateChanges, Store},
    sync::{JoinedRoomUpdate, LeftRoomUpdate, Notification, RoomUpdates, SyncResponse},
    Room, RoomInfo,
};

impl BaseClient {
    #[cfg(feature = "e2e-encryption")]
    /// Processes the E2EE-related events from the Sliding Sync response.
    ///
    /// In addition to writes to the crypto store, this may also write into the
    /// state store, in particular it may write latest-events to the state
    /// store.
    pub async fn process_sliding_sync_e2ee(
        &self,
        extensions: &v4::Extensions,
    ) -> Result<Vec<Raw<AnyToDeviceEvent>>> {
        if extensions.is_empty() {
            return Ok(Default::default());
        }

        let v4::Extensions { to_device, e2ee, .. } = extensions;

        let to_device_events = to_device.as_ref().map(|v4| v4.events.clone()).unwrap_or_default();

        trace!(
            to_device_events = to_device_events.len(),
            device_one_time_keys_count = e2ee.device_one_time_keys_count.len(),
            device_unused_fallback_key_types =
                e2ee.device_unused_fallback_key_types.as_ref().map(|v| v.len()),
            "Processing sliding sync e2ee events",
        );

        let mut changes = StateChanges::default();

        // Process the to-device events and other related e2ee data. This returns a list
        // of all the to-device events that were passed in but encrypted ones
        // were replaced with their decrypted version.
        // Passing in the default empty maps and vecs for this is completely fine, since
        // the `OlmMachine` assumes empty maps/vecs mean no change in the one-time key
        // counts.
        let to_device = self
            .preprocess_to_device_events(
                matrix_sdk_crypto::EncryptionSyncChanges {
                    to_device_events,
                    changed_devices: &e2ee.device_lists,
                    one_time_keys_counts: &e2ee.device_one_time_keys_count,
                    unused_fallback_keys: e2ee.device_unused_fallback_key_types.as_deref(),
                    next_batch_token: to_device
                        .as_ref()
                        .map(|to_device| to_device.next_batch.clone()),
                },
                &mut changes,
            )
            .await?;

        trace!("ready to submit changes to store");
        self.store.save_changes(&changes).await?;
        self.apply_changes(&changes, true);
        trace!("applied changes");

        Ok(to_device)
    }

    /// Process a response from a sliding sync call.
    ///
    /// # Arguments
    ///
    /// * `response` - The response that we received after a successful sliding
    ///   sync.
    #[instrument(skip_all, level = "trace")]
    pub async fn process_sliding_sync<PEP: PreviousEventsProvider>(
        &self,
        response: &v4::Response,
        previous_events_provider: &PEP,
    ) -> Result<SyncResponse> {
        let v4::Response {
            // FIXME not yet supported by sliding sync. see
            // https://github.com/matrix-org/matrix-rust-sdk/issues/1014
            // next_batch,
            rooms,
            lists,
            extensions,
            // FIXME: missing compared to v3::Response
            //presence,
            ..
        } = response;

        trace!(
            rooms = rooms.len(),
            lists = lists.len(),
            extensions = !extensions.is_empty(),
            "Processing sliding sync room events"
        );

        if rooms.is_empty() && extensions.is_empty() {
            // we received a room reshuffling event only, there won't be anything for us to
            // process. stop early
            return Ok(SyncResponse::default());
        };

        let mut changes = StateChanges::default();

        let store = self.store.clone();
        let mut ambiguity_cache = AmbiguityCache::new(store.inner.clone());

        let account_data = &extensions.account_data;
        if !account_data.is_empty() {
            self.handle_account_data(&account_data.global, &mut changes).await;
        }

        let mut new_rooms = RoomUpdates::default();
        let mut notifications = Default::default();
        let mut rooms_account_data = account_data.rooms.clone();

        for (room_id, response_room_data) in rooms {
            let (room_info, joined_room, left_room, invited_room) = self
                .process_sliding_sync_room(
                    room_id,
                    response_room_data,
                    &mut rooms_account_data,
                    &store,
                    &mut changes,
                    &mut notifications,
                    &mut ambiguity_cache,
                )
                .await?;

            changes.add_room(room_info);

            if let Some(joined_room) = joined_room {
                new_rooms.join.insert(room_id.clone(), joined_room);
            }

            if let Some(left_room) = left_room {
                new_rooms.leave.insert(room_id.clone(), left_room);
            }

            if let Some(invited_room) = invited_room {
                new_rooms.invite.insert(room_id.clone(), invited_room);
            }
        }

        // Handle read receipts and typing notifications independently of the rooms:
        // these both live in a different subsection of the server's response,
        // so they may exist without any update for the associated room.

        for (room_id, raw) in &extensions.receipts.rooms {
            match raw.deserialize() {
                Ok(event) => {
                    changes.add_receipts(room_id, event.content);
                }
                Err(e) => {
                    let event_id: Option<String> = raw.get_field("event_id").ok().flatten();
                    #[rustfmt::skip]
                    warn!(
                        ?room_id, event_id,
                        "Failed to deserialize read receipt room event: {e}"
                    );
                }
            }

            // We assume this can only happen in joined rooms, or something's very wrong.
            new_rooms
                .join
                .entry(room_id.to_owned())
                .or_insert_with(JoinedRoomUpdate::default)
                .ephemeral
                .push(raw.clone().cast());
        }

        for (room_id, raw) in &extensions.typing.rooms {
            // We assume this can only happen in joined rooms, or something's very wrong.
            new_rooms
                .join
                .entry(room_id.to_owned())
                .or_insert_with(JoinedRoomUpdate::default)
                .ephemeral
                .push(raw.clone().cast());
        }

        // Handle room account data
        for (room_id, raw) in &rooms_account_data {
            self.handle_room_account_data(room_id, raw, &mut changes).await;

            if let Some(room) = self.store.get_room(room_id) {
                match room.state() {
                    RoomState::Joined => new_rooms
                        .join
                        .entry(room_id.to_owned())
                        .or_insert_with(JoinedRoomUpdate::default)
                        .account_data
                        .append(&mut raw.to_vec()),
                    RoomState::Left => new_rooms
                        .leave
                        .entry(room_id.to_owned())
                        .or_insert_with(LeftRoomUpdate::default)
                        .account_data
                        .append(&mut raw.to_vec()),
                    RoomState::Invited => {}
                }
            }
        }

        // Rooms in `new_rooms.join` either have a timeline update, or a new read
        // receipt. Update the read receipt accordingly.
        let user_id = &self.session_meta().expect("logged in user").user_id;

        for (room_id, joined_room_update) in &mut new_rooms.join {
            if let Some(mut room_info) = changes
                .room_infos
                .get(room_id)
                .cloned()
                .or_else(|| self.get_room(room_id).map(|r| r.clone_info()))
            {
                let prev_read_receipts = room_info.read_receipts.clone();

                compute_unread_counts(
                    user_id,
                    room_id,
                    changes.receipts.get(room_id),
                    previous_events_provider.for_room(room_id),
                    &joined_room_update.timeline.events,
                    &mut room_info.read_receipts,
                );

                if prev_read_receipts != room_info.read_receipts {
                    changes.add_room(room_info);
                }
            }
        }

        // TODO remove this, we're processing account data events here again
        // because we want to have the push rules in place before we process
        // rooms and their events, but we want to create the rooms before we
        // process the `m.direct` account data event.
        if !account_data.is_empty() {
            self.handle_account_data(&account_data.global, &mut changes).await;
        }

        // FIXME not yet supported by sliding sync.
        // changes.presence = presence
        //     .events
        //     .iter()
        //     .filter_map(|e| {
        //         let event = e.deserialize().ok()?;
        //         Some((event.sender, e.clone()))
        //     })
        //     .collect();

        changes.ambiguity_maps = ambiguity_cache.cache;

        trace!("ready to submit changes to store");
        store.save_changes(&changes).await?;
        self.apply_changes(&changes, false);
        trace!("applied changes");

        Ok(SyncResponse {
            rooms: new_rooms,
            notifications,
            // FIXME not yet supported by sliding sync.
            presence: Default::default(),
            account_data: account_data.global.clone(),
            to_device: Default::default(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_sliding_sync_room(
        &self,
        room_id: &RoomId,
        room_data: &v4::SlidingSyncRoom,
        rooms_account_data: &mut BTreeMap<OwnedRoomId, Vec<Raw<AnyRoomAccountDataEvent>>>,
        store: &Store,
        changes: &mut StateChanges,
        notifications: &mut BTreeMap<OwnedRoomId, Vec<Notification>>,
        ambiguity_cache: &mut AmbiguityCache,
    ) -> Result<(RoomInfo, Option<JoinedRoomUpdate>, Option<LeftRoomUpdate>, Option<InvitedRoom>)>
    {
        let (raw_state_events, state_events): (Vec<_>, Vec<_>) = {
            let mut state_events = Vec::new();

            // Read state events from the `required_state` field.
            state_events.extend(Self::deserialize_state_events(&room_data.required_state));

            // Read state events from the `timeline` field.
            state_events.extend(Self::deserialize_state_events_from_timeline(&room_data.timeline));

            state_events.into_iter().unzip()
        };

        // Find or create the room in the store
        #[allow(unused_mut)] // Required for some feature flag combinations
        let (mut room, mut room_info, invited_room) =
            self.process_sliding_sync_room_membership(room_data, &state_events, store, room_id);

        room_info.mark_state_partially_synced();

        let mut user_ids = if !state_events.is_empty() {
            self.handle_state(
                &raw_state_events,
                &state_events,
                &mut room_info,
                changes,
                ambiguity_cache,
            )
            .await?
        } else {
            Default::default()
        };

        let push_rules = self.get_push_rules(changes).await?;

        if let Some(invite_state) = &room_data.invite_state {
            self.handle_invited_state(
                &room,
                invite_state,
                &push_rules,
                &mut room_info,
                changes,
                notifications,
            )
            .await?;
        }

        process_room_properties(room_data, &mut room_info);

        let timeline = self
            .handle_timeline(
                &room,
                room_data.limited,
                room_data.timeline.clone(),
                room_data.prev_batch.clone(),
                &push_rules,
                &mut user_ids,
                &mut room_info,
                changes,
                notifications,
                ambiguity_cache,
            )
            .await?;

        // Cache the latest decrypted event in room_info, and also keep any later
        // encrypted events, so we can slot them in when we get the keys.
        #[cfg(feature = "e2e-encryption")]
        cache_latest_events(&room, &mut room_info, &timeline.events, Some(changes), Some(store))
            .await;

        #[cfg(feature = "e2e-encryption")]
        if room_info.is_encrypted() {
            if let Some(o) = self.olm_machine().await.as_ref() {
                if !room.is_encrypted() {
                    // The room turned on encryption in this sync, we need
                    // to also get all the existing users and mark them for
                    // tracking.
                    let user_ids = store.get_user_ids(room_id, RoomMemberships::ACTIVE).await?;
                    o.update_tracked_users(user_ids.iter().map(Deref::deref)).await?
                }

                if !user_ids.is_empty() {
                    o.update_tracked_users(user_ids.iter().map(Deref::deref)).await?;
                }
            }
        }

        let notification_count = room_data.unread_notifications.clone().into();
        room_info.update_notification_count(notification_count);

        let ambiguity_changes = ambiguity_cache.changes.remove(room_id).unwrap_or_default();
        let room_account_data = rooms_account_data.get(room_id).cloned();

        match room_info.state() {
            RoomState::Joined => {
                // Ephemeral events are added separately, because we might not
                // have a room subsection in the response, yet we may have receipts for
                // that room.
                let ephemeral = Vec::new();

                Ok((
                    room_info,
                    Some(JoinedRoomUpdate::new(
                        timeline,
                        raw_state_events,
                        room_account_data.unwrap_or_default(),
                        ephemeral,
                        notification_count,
                        ambiguity_changes,
                    )),
                    None,
                    None,
                ))
            }

            RoomState::Left => Ok((
                room_info,
                None,
                Some(LeftRoomUpdate::new(
                    timeline,
                    raw_state_events,
                    room_account_data.unwrap_or_default(),
                    ambiguity_changes,
                )),
                None,
            )),

            RoomState::Invited => Ok((room_info, None, None, invited_room)),
        }
    }

    /// Look through the sliding sync data for this room, find/create it in the
    /// store, and process any invite information.
    /// If any invite_state exists, we take it to mean that we are invited to
    /// this room, unless that state contains membership events that specify
    /// otherwise. https://github.com/matrix-org/matrix-spec-proposals/blob/kegan/sync-v3/proposals/3575-sync.md#room-list-parameters
    fn process_sliding_sync_room_membership(
        &self,
        room_data: &v4::SlidingSyncRoom,
        state_events: &[AnySyncStateEvent],
        store: &Store,
        room_id: &RoomId,
    ) -> (Room, RoomInfo, Option<InvitedRoom>) {
        if let Some(invite_state) = &room_data.invite_state {
            let room = store.get_or_create_room(
                room_id,
                RoomState::Invited,
                self.roominfo_update_sender.clone(),
            );
            let mut room_info = room.clone_info();

            // We don't actually know what events are inside invite_state. In theory, they
            // might not contain an m.room.member event, or they might set the
            // membership to something other than invite. This would be very
            // weird behaviour by the server, because invite_state is supposed
            // to contain an m.room.member. We will call handle_invited_state, which will
            // reflect any information found in the real events inside
            // invite_state, but we default to considering this room invited
            // simply because invite_state exists. This is needed in the normal
            // case, because the sliding sync server tries to send minimal state,
            // meaning that we normally actually just receive {"type": "m.room.member"} with
            // no content at all.
            room_info.mark_as_invited();

            (room, room_info, Some(InvitedRoom::from(v3::InviteState::from(invite_state.clone()))))
        } else {
            let room = store.get_or_create_room(
                room_id,
                RoomState::Joined,
                self.roominfo_update_sender.clone(),
            );
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
            self.handle_own_room_membership(state_events, &mut room_info);

            (room, room_info, None)
        }
    }

    /// Find any m.room.member events that refer to the current user, and update
    /// the state in room_info to reflect the "membership" property.
    pub(crate) fn handle_own_room_membership(
        &self,
        state_events: &[AnySyncStateEvent],
        room_info: &mut RoomInfo,
    ) {
        let Some(meta) = self.session_meta() else {
            return;
        };

        // Start from the last event; the first membership event we see in that order is
        // the last in the regular order, so that's the only one we need to
        // consider.
        for event in state_events.iter().rev() {
            if let AnySyncStateEvent::RoomMember(member) = &event {
                // If this event updates the current user's membership, record that in the
                // room_info.
                if member.state_key() == meta.user_id.as_str() {
                    room_info.set_state(member.membership().into());
                    break;
                }
            }
        }
    }

    pub(crate) fn deserialize_state_events_from_timeline(
        raw_events: &[Raw<AnySyncTimelineEvent>],
    ) -> Vec<(Raw<AnySyncStateEvent>, AnySyncStateEvent)> {
        raw_events
            .iter()
            .filter_map(|raw_event| {
                // If it contains `state_key`, we assume it's a state event.
                if raw_event.get_field::<serde::de::IgnoredAny>("state_key").transpose().is_some() {
                    match raw_event.deserialize_as::<AnySyncStateEvent>() {
                        Ok(event) => {
                            // SAFETY: Casting `AnySyncTimelineEvent` to `AnySyncStateEvent` is safe
                            // because we checked that there is a `state_key`.
                            Some((raw_event.clone().cast(), event))
                        }

                        Err(error) => {
                            warn!("Couldn't deserialize state event from timeline: {error}");
                            None
                        }
                    }
                } else {
                    None
                }
            })
            .collect()
    }
}

/// Find the most recent decrypted event and cache it in the supplied RoomInfo.
///
/// If any encrypted events are found after that one, store them in the RoomInfo
/// too so we can use them when we get the relevant keys.
///
/// It is the responsibility of the caller to update the `RoomInfo` instance
/// stored in the `Room`.
#[cfg(feature = "e2e-encryption")]
async fn cache_latest_events(
    room: &Room,
    room_info: &mut RoomInfo,
    events: &[SyncTimelineEvent],
    changes: Option<&StateChanges>,
    store: Option<&Store>,
) {
    let mut encrypted_events =
        Vec::with_capacity(room.latest_encrypted_events.read().unwrap().capacity());

    for event in events.iter().rev() {
        if let Ok(timeline_event) = event.event.deserialize() {
            match is_suitable_for_latest_event(&timeline_event) {
                PossibleLatestEvent::YesRoomMessage(_)
                | PossibleLatestEvent::YesPoll(_)
                | PossibleLatestEvent::YesCallInvite(_)
                | PossibleLatestEvent::YesCallNotify(_) => {
                    // We found a suitable latest event. Store it.

                    // In order to make the latest event fast to read, we want to keep the
                    // associated sender in cache. This is a best-effort to gather enough
                    // information for creating a user profile as fast as possible. If information
                    // are missing, let's go back on the “slow” path.

                    let mut sender_profile = None;
                    let mut sender_name_is_ambiguous = None;

                    // First off, look up the sender's profile from the `StateChanges`, they are
                    // likely to be the most recent information.
                    if let Some(changes) = changes {
                        sender_profile = changes
                            .profiles
                            .get(room.room_id())
                            .and_then(|profiles_by_user| {
                                profiles_by_user.get(timeline_event.sender())
                            })
                            .cloned();

                        if let Some(sender_profile) = sender_profile.as_ref() {
                            sender_name_is_ambiguous = sender_profile
                                .as_original()
                                .and_then(|profile| profile.content.displayname.as_ref())
                                .and_then(|display_name| {
                                    changes.ambiguity_maps.get(room.room_id()).and_then(
                                        |map_for_room| {
                                            map_for_room
                                                .get(display_name)
                                                .map(|user_ids| user_ids.len() > 1)
                                        },
                                    )
                                });
                        }
                    }

                    // Otherwise, look up the sender's profile from the `Store`.
                    if sender_profile.is_none() {
                        if let Some(store) = store {
                            sender_profile = store
                                .get_profile(room.room_id(), timeline_event.sender())
                                .await
                                .ok()
                                .flatten();

                            // TODO: need to update `sender_name_is_ambiguous`,
                            // but how?
                        }
                    }

                    let latest_event = Box::new(LatestEvent::new_with_sender_details(
                        event.clone(),
                        sender_profile,
                        sender_name_is_ambiguous,
                    ));

                    // Store it in the return RoomInfo (it will be saved for us in the room later).
                    room_info.latest_event = Some(latest_event.clone());
                    // We don't need any of the older encrypted events because we have a new
                    // decrypted one.
                    room.latest_encrypted_events.write().unwrap().clear();
                    // We can stop looking through the timeline now because everything else is
                    // older.
                    break;
                }
                PossibleLatestEvent::NoEncrypted => {
                    // m.room.encrypted - this might be the latest event later - we can't tell until
                    // we are able to decrypt it, so store it for now
                    //
                    // Check how many encrypted events we have seen. Only store another if we
                    // haven't already stored the maximum number.
                    if encrypted_events.len() < encrypted_events.capacity() {
                        encrypted_events.push(event.event.clone());
                    }
                }
                _ => {
                    // Ignore unsuitable events
                }
            }
        } else {
            warn!(
                "Failed to deserialize event as AnySyncTimelineEvent. ID={}",
                event.event_id().expect("Event has no ID!")
            );
        }
    }

    // Push the encrypted events we found into the Room, in reverse order, so
    // the latest is last
    room.latest_encrypted_events.write().unwrap().extend(encrypted_events.into_iter().rev());
}

fn process_room_properties(room_data: &v4::SlidingSyncRoom, room_info: &mut RoomInfo) {
    // Handle the room's avatar.
    //
    // It can be updated via the state events, or via the
    // [`v4::SlidingSyncRoom::avatar`] field. This part of the code handles the
    // latter case. The former case is handled by [`BaseClient::handle_state`].
    match &room_data.avatar {
        // A new avatar!
        JsOption::Some(avatar_uri) => room_info.update_avatar(Some(avatar_uri.to_owned())),
        // Avatar must be removed.
        JsOption::Null => room_info.update_avatar(None),
        // Nothing to do.
        JsOption::Undefined => {}
    }

    // Sliding sync doesn't have a room summary, nevertheless it contains the joined
    // and invited member counts, in addition to the heroes if it's been configured
    // to return them (see the [`v4::RoomSubscription::include_heroes`]).
    if let Some(count) = room_data.joined_count {
        room_info.update_joined_member_count(count.into());
    }
    if let Some(count) = room_data.invited_count {
        room_info.update_invited_member_count(count.into());
    }

    if let Some(heroes) = &room_data.heroes {
        // Filter out all the heroes which don't have a user id or name.
        room_info.update_heroes(
            heroes.iter().filter_map(|hero| hero.user_id.as_ref()).cloned().collect(),
            heroes.iter().filter_map(|hero| hero.name.as_ref()).cloned().collect(),
        );
    }

    room_info.set_prev_batch(room_data.prev_batch.as_deref());

    if room_data.limited {
        room_info.mark_members_missing();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashSet},
        sync::{Arc, RwLock as SyncRwLock},
    };

    use assert_matches::assert_matches;
    use matrix_sdk_common::{deserialized_responses::SyncTimelineEvent, ring_buffer::RingBuffer};
    use matrix_sdk_test::async_test;
    use ruma::{
        api::client::sync::sync_events::{v4, UnreadNotificationsCount},
        assign, event_id,
        events::{
            direct::DirectEventContent,
            room::{
                avatar::RoomAvatarEventContent,
                canonical_alias::RoomCanonicalAliasEventContent,
                member::{MembershipState, RoomMemberEventContent},
                message::SyncRoomMessageEvent,
            },
            AnySyncMessageLikeEvent, AnySyncTimelineEvent, GlobalAccountDataEventContent,
            StateEventContent,
        },
        mxc_uri, room_alias_id, room_id,
        serde::Raw,
        uint, user_id, JsOption, MxcUri, OwnedRoomId, OwnedUserId, RoomAliasId, RoomId, UserId,
    };
    use serde_json::json;

    use super::cache_latest_events;
    use crate::{
        store::MemoryStore, test_utils::logged_in_base_client, BaseClient, Room, RoomState,
    };

    #[async_test]
    async fn test_notification_count_set() {
        let client = logged_in_base_client(None).await;

        let mut response = v4::Response::new("42".to_owned());
        let room_id = room_id!("!room:example.org");
        let count = assign!(UnreadNotificationsCount::default(), {
            highlight_count: Some(uint!(13)),
            notification_count: Some(uint!(37)),
        });

        response.rooms.insert(
            room_id.to_owned(),
            assign!(v4::SlidingSyncRoom::new(), {
                unread_notifications: count.clone()
            }),
        );

        let sync_response =
            client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");

        // Check it's present in the response.
        let room = sync_response.rooms.join.get(room_id).unwrap();
        assert_eq!(room.unread_notifications, count.clone().into());

        // Check it's been updated in the store.
        let room = client.get_room(room_id).expect("found room");
        assert_eq!(room.unread_notification_counts(), count.into());
    }

    #[async_test]
    async fn test_can_process_empty_sliding_sync_response() {
        let client = logged_in_base_client(None).await;
        let empty_response = v4::Response::new("5".to_owned());
        client.process_sliding_sync(&empty_response, &()).await.expect("Failed to process sync");
    }

    #[async_test]
    async fn test_room_with_unspecified_state_is_added_to_client_and_joined_list() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");

        // When I send sliding sync response containing a room (with identifiable data
        // in joined_count)
        let mut room = v4::SlidingSyncRoom::new();
        room.joined_count = Some(uint!(41));
        let response = response_with_room(room_id, room);
        let sync_resp =
            client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");

        // Then the room appears in the client (with the same joined count)
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(client_room.room_id(), room_id);
        assert_eq!(client_room.joined_members_count(), 41);
        assert_eq!(client_room.state(), RoomState::Joined);

        // And it is added to the list of joined rooms only.
        assert!(sync_resp.rooms.join.contains_key(room_id));
        assert!(!sync_resp.rooms.leave.contains_key(room_id));
        assert!(!sync_resp.rooms.invite.contains_key(room_id));
    }

    #[async_test]
    async fn test_room_name_is_found_when_processing_sliding_sync_response() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");

        // When I send sliding sync response containing a room with a name
        let mut room = v4::SlidingSyncRoom::new();
        room.name = Some("little room".to_owned());
        let response = response_with_room(room_id, room);
        let sync_resp =
            client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");

        // Then the room doesn't have the name in the client.
        let client_room = client.get_room(room_id).expect("No room found");
        assert!(client_room.name().is_none());
        assert_eq!(client_room.computed_display_name().await.unwrap().to_string(), "Empty Room");
        assert_eq!(client_room.state(), RoomState::Joined);

        // And it is added to the list of joined rooms only.
        assert!(sync_resp.rooms.join.contains_key(room_id));
        assert!(!sync_resp.rooms.leave.contains_key(room_id));
        assert!(!sync_resp.rooms.invite.contains_key(room_id));
    }

    #[async_test]
    async fn test_invited_room_name_is_found_when_processing_sliding_sync_response() {
        // Given a logged-in client,
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");
        let user_id = user_id!("@w:e.uk");
        let inviter = user_id!("@john:mastodon.org");

        // When I send sliding sync response containing a room with a name,
        let mut room = v4::SlidingSyncRoom::new();
        set_room_invited(&mut room, inviter, user_id);
        room.name = Some("name from sliding sync response".to_owned());
        let response = response_with_room(room_id, room);
        let sync_resp =
            client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");

        // Then the room doesn't have the name in the client.
        let client_room = client.get_room(room_id).expect("No room found");
        assert!(client_room.name().is_none());

        // The computed display name will show heroes name, but for an invited room…
        // it's only self, the invitee, who will appear.
        assert_eq!(client_room.computed_display_name().await.unwrap().to_string(), "w");

        assert_eq!(client_room.state(), RoomState::Invited);

        // And it is added to the list of invited rooms only.
        assert!(!sync_resp.rooms.join.contains_key(room_id));
        assert!(!sync_resp.rooms.leave.contains_key(room_id));
        assert!(sync_resp.rooms.invite.contains_key(room_id));
    }

    #[async_test]
    async fn test_left_a_room_from_required_state_event() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");
        let user_id = user_id!("@u:e.uk");

        // When I join…
        let mut room = v4::SlidingSyncRoom::new();
        set_room_joined(&mut room, user_id);
        let response = response_with_room(room_id, room);
        client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");
        assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Joined);

        // And then leave with a `required_state` state event…
        let mut room = v4::SlidingSyncRoom::new();
        set_room_left(&mut room, user_id);
        let response = response_with_room(room_id, room);
        let sync_resp =
            client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");

        // The room is left.
        assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Left);

        // And it is added to the list of left rooms only.
        assert!(!sync_resp.rooms.join.contains_key(room_id));
        assert!(sync_resp.rooms.leave.contains_key(room_id));
        assert!(!sync_resp.rooms.invite.contains_key(room_id));
    }

    #[async_test]
    async fn test_kick_or_ban_updates_room_to_left() {
        for membership in [MembershipState::Leave, MembershipState::Ban] {
            let room_id = room_id!("!r:e.uk");
            let user_a_id = user_id!("@a:e.uk");
            let user_b_id = user_id!("@b:e.uk");
            let client = logged_in_base_client(Some(user_a_id)).await;

            // When I join…
            let mut room = v4::SlidingSyncRoom::new();
            set_room_joined(&mut room, user_a_id);
            let response = response_with_room(room_id, room);
            client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");
            assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Joined);

            // And then get kicked/banned with a `required_state` state event…
            let mut room = v4::SlidingSyncRoom::new();
            room.required_state.push(make_state_event(
                user_b_id,
                user_a_id.as_str(),
                RoomMemberEventContent::new(membership),
                None,
            ));
            let response = response_with_room(room_id, room);
            let sync_resp =
                client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");

            // The room is left.
            assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Left);

            // And it is added to the list of left rooms only.
            assert!(!sync_resp.rooms.join.contains_key(room_id));
            assert!(sync_resp.rooms.leave.contains_key(room_id));
            assert!(!sync_resp.rooms.invite.contains_key(room_id));
        }
    }

    #[async_test]
    async fn test_left_a_room_from_timeline_state_event() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");
        let user_id = user_id!("@u:e.uk");

        // When I join…
        let mut room = v4::SlidingSyncRoom::new();
        set_room_joined(&mut room, user_id);
        let response = response_with_room(room_id, room);
        client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");
        assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Joined);

        // And then leave with a `timeline` state event…
        let mut room = v4::SlidingSyncRoom::new();
        set_room_left_as_timeline_event(&mut room, user_id);
        let response = response_with_room(room_id, room);
        client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");

        // The room is left.
        assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Left);
    }

    #[async_test]
    async fn test_can_be_reinvited_to_a_left_room() {
        // See https://github.com/matrix-org/matrix-rust-sdk/issues/1834

        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");
        let user_id = user_id!("@u:e.uk");

        // When I join...
        let mut room = v4::SlidingSyncRoom::new();
        set_room_joined(&mut room, user_id);
        let response = response_with_room(room_id, room);
        client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");
        // (sanity: state is join)
        assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Joined);

        // And then leave...
        let mut room = v4::SlidingSyncRoom::new();
        set_room_left(&mut room, user_id);
        let response = response_with_room(room_id, room);
        client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");
        // (sanity: state is left)
        assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Left);

        // And then get invited back
        let mut room = v4::SlidingSyncRoom::new();
        set_room_invited(&mut room, user_id, user_id);
        let response = response_with_room(room_id, room);
        client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");

        // Then the room is in the invite state
        assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Invited);
    }

    #[async_test]
    async fn test_other_person_leaving_a_dm_is_reflected_in_their_membership_and_direct_targets() {
        let room_id = room_id!("!r:e.uk");
        let user_a_id = user_id!("@a:e.uk");
        let user_b_id = user_id!("@b:e.uk");

        // Given we have a DM with B, who is joined
        let client = logged_in_base_client(None).await;
        create_dm(&client, room_id, user_a_id, user_b_id, MembershipState::Join).await;

        // (Sanity: B is a direct target, and is in Join state)
        assert!(direct_targets(&client, room_id).contains(user_b_id));
        assert_eq!(membership(&client, room_id, user_b_id).await, MembershipState::Join);

        // When B leaves
        update_room_membership(&client, room_id, user_b_id, MembershipState::Leave).await;

        // Then B is still a direct target, and is in Leave state (B is a direct target
        // because we want to return to our old DM in the UI even if the other
        // user left, so we can reinvite them. See https://github.com/matrix-org/matrix-rust-sdk/issues/2017)
        assert!(direct_targets(&client, room_id).contains(user_b_id));
        assert_eq!(membership(&client, room_id, user_b_id).await, MembershipState::Leave);
    }

    #[async_test]
    async fn test_other_person_refusing_invite_to_a_dm_is_reflected_in_their_membership_and_direct_targets(
    ) {
        let room_id = room_id!("!r:e.uk");
        let user_a_id = user_id!("@a:e.uk");
        let user_b_id = user_id!("@b:e.uk");

        // Given I have invited B to a DM
        let client = logged_in_base_client(None).await;
        create_dm(&client, room_id, user_a_id, user_b_id, MembershipState::Invite).await;

        // (Sanity: B is a direct target, and is in Invite state)
        assert!(direct_targets(&client, room_id).contains(user_b_id));
        assert_eq!(membership(&client, room_id, user_b_id).await, MembershipState::Invite);

        // When B declines the invitation (i.e. leaves)
        update_room_membership(&client, room_id, user_b_id, MembershipState::Leave).await;

        // Then B is still a direct target, and is in Leave state (B is a direct target
        // because we want to return to our old DM in the UI even if the other
        // user left, so we can reinvite them. See https://github.com/matrix-org/matrix-rust-sdk/issues/2017)
        assert!(direct_targets(&client, room_id).contains(user_b_id));
        assert_eq!(membership(&client, room_id, user_b_id).await, MembershipState::Leave);
    }

    #[async_test]
    async fn test_members_count_in_a_dm_where_other_person_has_joined() {
        let room_id = room_id!("!r:bar.org");
        let user_a_id = user_id!("@a:bar.org");
        let user_b_id = user_id!("@b:bar.org");

        // Given we have a DM with B, who is joined
        let client = logged_in_base_client(None).await;
        create_dm(&client, room_id, user_a_id, user_b_id, MembershipState::Join).await;

        // (Sanity: A is in Join state)
        assert_eq!(membership(&client, room_id, user_a_id).await, MembershipState::Join);

        // (Sanity: B is a direct target, and is in Join state)
        assert!(direct_targets(&client, room_id).contains(user_b_id));
        assert_eq!(membership(&client, room_id, user_b_id).await, MembershipState::Join);

        let room = client.get_room(room_id).unwrap();

        assert_eq!(room.active_members_count(), 2);
        assert_eq!(room.joined_members_count(), 2);
        assert_eq!(room.invited_members_count(), 0);
    }

    #[async_test]
    async fn test_members_count_in_a_dm_where_other_person_is_invited() {
        let room_id = room_id!("!r:bar.org");
        let user_a_id = user_id!("@a:bar.org");
        let user_b_id = user_id!("@b:bar.org");

        // Given we have a DM with B, who is joined
        let client = logged_in_base_client(None).await;
        create_dm(&client, room_id, user_a_id, user_b_id, MembershipState::Invite).await;

        // (Sanity: A is in Join state)
        assert_eq!(membership(&client, room_id, user_a_id).await, MembershipState::Join);

        // (Sanity: B is a direct target, and is in Join state)
        assert!(direct_targets(&client, room_id).contains(user_b_id));
        assert_eq!(membership(&client, room_id, user_b_id).await, MembershipState::Invite);

        let room = client.get_room(room_id).unwrap();

        assert_eq!(room.active_members_count(), 2);
        assert_eq!(room.joined_members_count(), 1);
        assert_eq!(room.invited_members_count(), 1);
    }

    #[async_test]
    async fn test_avatar_is_found_when_processing_sliding_sync_response() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");

        // When I send sliding sync response containing a room with an avatar
        let room = {
            let mut room = v4::SlidingSyncRoom::new();
            room.avatar = JsOption::from_option(Some(mxc_uri!("mxc://e.uk/med1").to_owned()));

            room
        };
        let response = response_with_room(room_id, room);
        client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");

        // Then the room in the client has the avatar
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(
            client_room.avatar_url().expect("No avatar URL").media_id().expect("No media ID"),
            "med1"
        );
    }

    #[async_test]
    async fn test_avatar_can_be_unset_when_processing_sliding_sync_response() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");

        // Set the avatar.

        // When I send sliding sync response containing a room with an avatar
        let room = {
            let mut room = v4::SlidingSyncRoom::new();
            room.avatar = JsOption::from_option(Some(mxc_uri!("mxc://e.uk/med1").to_owned()));

            room
        };
        let response = response_with_room(room_id, room);
        client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");

        // Then the room in the client has the avatar
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(
            client_room.avatar_url().expect("No avatar URL").media_id().expect("No media ID"),
            "med1"
        );

        // No avatar. Still here.

        // When I send sliding sync response containing no avatar.
        let room = v4::SlidingSyncRoom::new();
        let response = response_with_room(room_id, room);
        client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");

        // Then the room in the client still has the avatar
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(
            client_room.avatar_url().expect("No avatar URL").media_id().expect("No media ID"),
            "med1"
        );

        // Avatar is unset.

        // When I send sliding sync response containing an avatar set to `null` (!).
        let room = {
            let mut room = v4::SlidingSyncRoom::new();
            room.avatar = JsOption::Null;

            room
        };
        let response = response_with_room(room_id, room);
        client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");

        // Then the room in the client has no more avatar
        let client_room = client.get_room(room_id).expect("No room found");
        assert!(client_room.avatar_url().is_none());
    }

    #[async_test]
    async fn test_avatar_is_found_from_required_state_when_processing_sliding_sync_response() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");
        let user_id = user_id!("@u:e.uk");

        // When I send sliding sync response containing a room with an avatar
        let room = room_with_avatar(mxc_uri!("mxc://e.uk/med1"), user_id);
        let response = response_with_room(room_id, room);
        client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");

        // Then the room in the client has the avatar
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(
            client_room.avatar_url().expect("No avatar URL").media_id().expect("No media ID"),
            "med1"
        );
    }

    #[async_test]
    async fn test_invitation_room_is_added_to_client_and_invite_list() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");
        let user_id = user_id!("@u:e.uk");

        // When I send sliding sync response containing an invited room
        let mut room = v4::SlidingSyncRoom::new();
        set_room_invited(&mut room, user_id, user_id);
        let response = response_with_room(room_id, room);
        let sync_resp =
            client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");

        // Then the room is added to the client
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(client_room.room_id(), room_id);
        assert_eq!(client_room.state(), RoomState::Invited);

        // And it is added to the list of invited rooms, not the joined ones
        assert!(!sync_resp.rooms.invite[room_id].invite_state.is_empty());
        assert!(!sync_resp.rooms.join.contains_key(room_id));
    }

    #[async_test]
    async fn test_avatar_is_found_in_invitation_room_when_processing_sliding_sync_response() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");
        let user_id = user_id!("@u:e.uk");

        // When I send sliding sync response containing an invited room with an avatar
        let mut room = room_with_avatar(mxc_uri!("mxc://e.uk/med1"), user_id);
        set_room_invited(&mut room, user_id, user_id);
        let response = response_with_room(room_id, room);
        client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");

        // Then the room in the client has the avatar
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(
            client_room.avatar_url().expect("No avatar URL").media_id().expect("No media ID"),
            "med1"
        );
    }

    #[async_test]
    async fn test_canonical_alias_is_found_in_invitation_room_when_processing_sliding_sync_response(
    ) {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");
        let user_id = user_id!("@u:e.uk");
        let room_alias_id = room_alias_id!("#myroom:e.uk");

        // When I send sliding sync response containing an invited room with an avatar
        let mut room = room_with_canonical_alias(room_alias_id, user_id);
        set_room_invited(&mut room, user_id, user_id);
        let response = response_with_room(room_id, room);
        client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");

        // Then the room in the client has the avatar
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(client_room.canonical_alias(), Some(room_alias_id.to_owned()));
    }

    #[async_test]
    async fn test_display_name_from_sliding_sync_doesnt_override_alias() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");
        let user_id = user_id!("@u:e.uk");
        let room_alias_id = room_alias_id!("#myroom:e.uk");

        // When the sliding sync response contains an explicit room name as well as an
        // alias
        let mut room = room_with_canonical_alias(room_alias_id, user_id);
        room.name = Some("This came from the server".to_owned());
        let response = response_with_room(room_id, room);
        client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");

        // Then the room's name is NOT overridden by the server-computed display name.
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(client_room.computed_display_name().await.unwrap().to_string(), "myroom");
        assert!(client_room.name().is_none());
    }

    #[async_test]
    async fn test_compute_heroes_from_sliding_sync() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");
        let gordon = user_id!("@gordon:e.uk").to_owned();
        let alice = user_id!("@alice:e.uk").to_owned();

        // When I send sliding sync response containing a room (with identifiable data
        // in `heroes`)
        let mut room = v4::SlidingSyncRoom::new();
        room.heroes = Some(vec![
            assign!(v4::SlidingSyncRoomHero::default(), {
                user_id: Some(gordon),
                name: Some("Gordon".to_owned()),
            }),
            assign!(v4::SlidingSyncRoomHero::default(), {
                user_id: Some(alice),
                name: Some("Alice".to_owned()),
            }),
        ]);
        let response = response_with_room(room_id, room);
        let _sync_resp =
            client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");

        // Then the room appears in the client.
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(client_room.room_id(), room_id);
        assert_eq!(client_room.state(), RoomState::Joined);

        // And heroes are part of the summary.
        assert_eq!(
            client_room.clone_info().summary.heroes(),
            &["@gordon:e.uk".to_owned(), "@alice:e.uk".to_owned()]
        );
    }

    #[async_test]
    async fn test_last_event_from_sliding_sync_is_cached() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");
        let event_a = json!({
            "sender":"@alice:example.com",
            "type":"m.room.message",
            "event_id": "$ida",
            "origin_server_ts": 12344446,
            "content":{"body":"A", "msgtype": "m.text"}
        });
        let event_b = json!({
            "sender":"@alice:example.com",
            "type":"m.room.message",
            "event_id": "$idb",
            "origin_server_ts": 12344447,
            "content":{"body":"B", "msgtype": "m.text"}
        });

        // When the sliding sync response contains a timeline
        let events = &[event_a, event_b.clone()];
        let room = room_with_timeline(events);
        let response = response_with_room(room_id, room);
        client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");

        // Then the room holds the latest event
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(
            ev_id(client_room.latest_event().map(|latest_event| latest_event.event().clone())),
            "$idb"
        );
    }

    #[async_test]
    async fn test_cached_latest_event_can_be_redacted() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");
        let event_a = json!({
            "sender": "@alice:example.com",
            "type": "m.room.message",
            "event_id": "$ida",
            "origin_server_ts": 12344446,
            "content": { "body":"A", "msgtype": "m.text" },
        });

        // When the sliding sync response contains a timeline
        let room = room_with_timeline(&[event_a]);
        let response = response_with_room(room_id, room);
        client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");

        // Then the room holds the latest event
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(
            ev_id(client_room.latest_event().map(|latest_event| latest_event.event().clone())),
            "$ida"
        );

        let redaction = json!({
            "sender": "@alice:example.com",
            "type": "m.room.redaction",
            "event_id": "$idb",
            "redacts": "$ida",
            "origin_server_ts": 12344448,
            "content": {},
        });

        // When a redaction for that event is received
        let room = room_with_timeline(&[redaction]);
        let response = response_with_room(room_id, room);
        client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");

        // Then the room still holds the latest event
        let client_room = client.get_room(room_id).expect("No room found");
        let latest_event = client_room.latest_event().unwrap();
        assert_eq!(latest_event.event_id().unwrap(), "$ida");

        // But it's now redacted
        assert_matches!(
            latest_event.event().event.deserialize().unwrap(),
            AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
                SyncRoomMessageEvent::Redacted(_)
            ))
        );
    }

    #[async_test]
    async fn test_when_no_events_we_dont_cache_any() {
        let events = &[];
        let chosen = choose_event_to_cache(events).await;
        assert!(chosen.is_none());
    }

    #[async_test]
    async fn test_when_only_one_event_we_cache_it() {
        let event1 = make_event("m.room.message", "$1");
        let events = &[event1.clone()];
        let chosen = choose_event_to_cache(events).await;
        assert_eq!(ev_id(chosen), rawev_id(event1));
    }

    #[async_test]
    async fn test_with_multiple_events_we_cache_the_last_one() {
        let event1 = make_event("m.room.message", "$1");
        let event2 = make_event("m.room.message", "$2");
        let events = &[event1, event2.clone()];
        let chosen = choose_event_to_cache(events).await;
        assert_eq!(ev_id(chosen), rawev_id(event2));
    }

    #[async_test]
    async fn test_cache_the_latest_relevant_event_and_ignore_irrelevant_ones_even_if_later() {
        let event1 = make_event("m.room.message", "$1");
        let event2 = make_event("m.room.message", "$2");
        let event3 = make_event("m.room.powerlevels", "$3");
        let event4 = make_event("m.room.powerlevels", "$5");
        let events = &[event1, event2.clone(), event3, event4];
        let chosen = choose_event_to_cache(events).await;
        assert_eq!(ev_id(chosen), rawev_id(event2));
    }

    #[async_test]
    async fn test_prefer_to_cache_nothing_rather_than_irrelevant_events() {
        let event1 = make_event("m.room.power_levels", "$1");
        let events = &[event1];
        let chosen = choose_event_to_cache(events).await;
        assert!(chosen.is_none());
    }

    #[async_test]
    async fn test_cache_encrypted_events_that_are_after_latest_message() {
        // Given two message events followed by two encrypted
        let event1 = make_event("m.room.message", "$1");
        let event2 = make_event("m.room.message", "$2");
        let event3 = make_encrypted_event("$3");
        let event4 = make_encrypted_event("$4");
        let events = &[event1, event2.clone(), event3.clone(), event4.clone()];

        // When I ask to cache events
        let room = make_room();
        let mut room_info = room.clone_info();
        cache_latest_events(&room, &mut room_info, events, None, None).await;

        // The latest message is stored
        assert_eq!(
            ev_id(room_info.latest_event.as_ref().map(|latest_event| latest_event.event().clone())),
            rawev_id(event2.clone())
        );

        room.set_room_info(room_info, false);
        assert_eq!(
            ev_id(room.latest_event().map(|latest_event| latest_event.event().clone())),
            rawev_id(event2)
        );

        // And also the two encrypted ones
        assert_eq!(rawevs_ids(&room.latest_encrypted_events), evs_ids(&[event3, event4]));
    }

    #[async_test]
    async fn test_dont_cache_encrypted_events_that_are_before_latest_message() {
        // Given an encrypted event before and after the message
        let event1 = make_encrypted_event("$1");
        let event2 = make_event("m.room.message", "$2");
        let event3 = make_encrypted_event("$3");
        let events = &[event1, event2.clone(), event3.clone()];

        // When I ask to cache events
        let room = make_room();
        let mut room_info = room.clone_info();
        cache_latest_events(&room, &mut room_info, events, None, None).await;
        room.set_room_info(room_info, false);

        // The latest message is stored
        assert_eq!(
            ev_id(room.latest_event().map(|latest_event| latest_event.event().clone())),
            rawev_id(event2)
        );

        // And also the encrypted one that was after it, but not the one before
        assert_eq!(rawevs_ids(&room.latest_encrypted_events), evs_ids(&[event3]));
    }

    #[async_test]
    async fn test_skip_irrelevant_events_eg_receipts_even_if_after_message() {
        // Given two message events followed by two encrypted, with a receipt in the
        // middle
        let event1 = make_event("m.room.message", "$1");
        let event2 = make_event("m.room.message", "$2");
        let event3 = make_encrypted_event("$3");
        let event4 = make_event("m.read", "$4");
        let event5 = make_encrypted_event("$5");
        let events = &[event1, event2.clone(), event3.clone(), event4, event5.clone()];

        // When I ask to cache events
        let room = make_room();
        let mut room_info = room.clone_info();
        cache_latest_events(&room, &mut room_info, events, None, None).await;
        room.set_room_info(room_info, false);

        // The latest message is stored, ignoring the receipt
        assert_eq!(
            ev_id(room.latest_event().map(|latest_event| latest_event.event().clone())),
            rawev_id(event2)
        );

        // The two encrypted ones are stored, but not the receipt
        assert_eq!(rawevs_ids(&room.latest_encrypted_events), evs_ids(&[event3, event5]));
    }

    #[async_test]
    async fn test_only_store_the_max_number_of_encrypted_events() {
        // Given two message events followed by lots of encrypted and other irrelevant
        // events
        let evente = make_event("m.room.message", "$e");
        let eventd = make_event("m.room.message", "$d");
        let eventc = make_encrypted_event("$c");
        let event9 = make_encrypted_event("$9");
        let event8 = make_encrypted_event("$8");
        let event7 = make_encrypted_event("$7");
        let eventb = make_event("m.read", "$b");
        let event6 = make_encrypted_event("$6");
        let event5 = make_encrypted_event("$5");
        let event4 = make_encrypted_event("$4");
        let event3 = make_encrypted_event("$3");
        let event2 = make_encrypted_event("$2");
        let eventa = make_event("m.read", "$a");
        let event1 = make_encrypted_event("$1");
        let event0 = make_encrypted_event("$0");
        let events = &[
            evente,
            eventd.clone(),
            eventc,
            event9.clone(),
            event8.clone(),
            event7.clone(),
            eventb,
            event6.clone(),
            event5.clone(),
            event4.clone(),
            event3.clone(),
            event2.clone(),
            eventa,
            event1.clone(),
            event0.clone(),
        ];

        // When I ask to cache events
        let room = make_room();
        let mut room_info = room.clone_info();
        cache_latest_events(&room, &mut room_info, events, None, None).await;
        room.set_room_info(room_info, false);

        // The latest message is stored, ignoring encrypted and receipts
        assert_eq!(
            ev_id(room.latest_event().map(|latest_event| latest_event.event().clone())),
            rawev_id(eventd)
        );

        // Only 10 encrypted are stored, even though there were more
        assert_eq!(
            rawevs_ids(&room.latest_encrypted_events),
            evs_ids(&[
                event9, event8, event7, event6, event5, event4, event3, event2, event1, event0
            ])
        );
    }

    #[async_test]
    async fn test_dont_overflow_capacity_if_previous_encrypted_events_exist() {
        // Given a RoomInfo with lots of encrypted events already inside it
        let room = make_room();
        let mut room_info = room.clone_info();
        cache_latest_events(
            &room,
            &mut room_info,
            &[
                make_encrypted_event("$0"),
                make_encrypted_event("$1"),
                make_encrypted_event("$2"),
                make_encrypted_event("$3"),
                make_encrypted_event("$4"),
                make_encrypted_event("$5"),
                make_encrypted_event("$6"),
                make_encrypted_event("$7"),
                make_encrypted_event("$8"),
                make_encrypted_event("$9"),
            ],
            None,
            None,
        )
        .await;
        room.set_room_info(room_info, false);

        // Sanity: room_info has 10 encrypted events inside it
        assert_eq!(room.latest_encrypted_events.read().unwrap().len(), 10);

        // When I ask to cache more encrypted events
        let eventa = make_encrypted_event("$a");
        let mut room_info = room.clone_info();
        cache_latest_events(&room, &mut room_info, &[eventa], None, None).await;
        room.set_room_info(room_info, false);

        // The oldest event is gone
        assert!(!rawevs_ids(&room.latest_encrypted_events).contains(&"$0".to_owned()));

        // The newest event is last in the list
        assert_eq!(rawevs_ids(&room.latest_encrypted_events)[9], "$a");
    }

    #[async_test]
    async fn test_existing_encrypted_events_are_deleted_if_we_receive_unencrypted() {
        // Given a RoomInfo with some encrypted events already inside it
        let room = make_room();
        let mut room_info = room.clone_info();
        cache_latest_events(
            &room,
            &mut room_info,
            &[make_encrypted_event("$0"), make_encrypted_event("$1"), make_encrypted_event("$2")],
            None,
            None,
        )
        .await;
        room.set_room_info(room_info.clone(), false);

        // When I ask to cache an unencrypted event, and some more encrypted events
        let eventa = make_event("m.room.message", "$a");
        let eventb = make_encrypted_event("$b");
        cache_latest_events(&room, &mut room_info, &[eventa, eventb], None, None).await;
        room.set_room_info(room_info, false);

        // The only encrypted events stored are the ones after the decrypted one
        assert_eq!(rawevs_ids(&room.latest_encrypted_events), &["$b"]);

        // The decrypted one is stored as the latest
        assert_eq!(rawev_id(room.latest_event().unwrap().event().clone()), "$a");
    }

    async fn choose_event_to_cache(events: &[SyncTimelineEvent]) -> Option<SyncTimelineEvent> {
        let room = make_room();
        let mut room_info = room.clone_info();
        cache_latest_events(&room, &mut room_info, events, None, None).await;
        room.set_room_info(room_info, false);
        room.latest_event().map(|latest_event| latest_event.event().clone())
    }

    fn rawev_id(event: SyncTimelineEvent) -> String {
        event.event_id().unwrap().to_string()
    }

    fn ev_id(event: Option<SyncTimelineEvent>) -> String {
        event.unwrap().event_id().unwrap().to_string()
    }

    fn rawevs_ids(events: &Arc<SyncRwLock<RingBuffer<Raw<AnySyncTimelineEvent>>>>) -> Vec<String> {
        events.read().unwrap().iter().map(|e| e.get_field("event_id").unwrap().unwrap()).collect()
    }

    fn evs_ids(events: &[SyncTimelineEvent]) -> Vec<String> {
        events.iter().map(|e| e.event_id().unwrap().to_string()).collect()
    }

    fn make_room() -> Room {
        let (sender, _receiver) = tokio::sync::broadcast::channel(1);

        Room::new(
            user_id!("@u:e.co"),
            Arc::new(MemoryStore::new()),
            room_id!("!r:e.co"),
            RoomState::Joined,
            sender,
        )
    }

    fn make_event(typ: &str, id: &str) -> SyncTimelineEvent {
        SyncTimelineEvent::new(
            Raw::from_json_string(
                json!({
                    "type": typ,
                    "event_id": id,
                    "content": { "msgtype": "m.text", "body": "my msg" },
                    "sender": "@u:h.uk",
                    "origin_server_ts": 12344445,
                })
                .to_string(),
            )
            .unwrap(),
        )
    }

    fn make_encrypted_event(id: &str) -> SyncTimelineEvent {
        SyncTimelineEvent::new(
            Raw::from_json_string(
                json!({
                    "type": "m.room.encrypted",
                    "event_id": id,
                    "content": {
                        "algorithm": "m.megolm.v1.aes-sha2",
                        "ciphertext": "",
                        "sender_key": "",
                        "device_id": "",
                        "session_id": "",
                    },
                    "sender": "@u:h.uk",
                    "origin_server_ts": 12344445,
                })
                .to_string(),
            )
            .unwrap(),
        )
    }

    async fn membership(
        client: &BaseClient,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> MembershipState {
        let room = client.get_room(room_id).expect("Room not found!");
        let member = room.get_member(user_id).await.unwrap().expect("B not in room");
        member.membership().clone()
    }

    fn direct_targets(client: &BaseClient, room_id: &RoomId) -> HashSet<OwnedUserId> {
        let room = client.get_room(room_id).expect("Room not found!");
        room.direct_targets()
    }

    /// Create a DM with the other user, setting our membership to Join and
    /// theirs to other_state
    async fn create_dm(
        client: &BaseClient,
        room_id: &RoomId,
        my_id: &UserId,
        their_id: &UserId,
        other_state: MembershipState,
    ) {
        let mut room = v4::SlidingSyncRoom::new();
        set_room_joined(&mut room, my_id);

        match other_state {
            MembershipState::Join => {
                room.joined_count = Some(uint!(2));
                room.invited_count = None;
            }

            MembershipState::Invite => {
                room.joined_count = Some(uint!(1));
                room.invited_count = Some(uint!(1));
            }

            _ => {
                room.joined_count = Some(uint!(1));
                room.invited_count = None;
            }
        }

        room.required_state.push(make_membership_event(their_id, other_state));

        let mut response = response_with_room(room_id, room);
        set_direct_with(&mut response, their_id.to_owned(), vec![room_id.to_owned()]);
        client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");
    }

    /// Set this user's membership within this room to new_state
    async fn update_room_membership(
        client: &BaseClient,
        room_id: &RoomId,
        user_id: &UserId,
        new_state: MembershipState,
    ) {
        let mut room = v4::SlidingSyncRoom::new();
        room.required_state.push(make_membership_event(user_id, new_state));
        let response = response_with_room(room_id, room);
        client.process_sliding_sync(&response, &()).await.expect("Failed to process sync");
    }

    fn set_direct_with(
        response: &mut v4::Response,
        user_id: OwnedUserId,
        room_ids: Vec<OwnedRoomId>,
    ) {
        let mut direct_content = BTreeMap::new();
        direct_content.insert(user_id, room_ids);
        response
            .extensions
            .account_data
            .global
            .push(make_global_account_data_event(DirectEventContent(direct_content)));
    }

    fn response_with_room(room_id: &RoomId, room: v4::SlidingSyncRoom) -> v4::Response {
        let mut response = v4::Response::new("5".to_owned());
        response.rooms.insert(room_id.to_owned(), room);
        response
    }

    fn room_with_avatar(avatar_uri: &MxcUri, user_id: &UserId) -> v4::SlidingSyncRoom {
        let mut room = v4::SlidingSyncRoom::new();

        let mut avatar_event_content = RoomAvatarEventContent::new();
        avatar_event_content.url = Some(avatar_uri.to_owned());

        room.required_state.push(make_state_event(user_id, "", avatar_event_content, None));

        room
    }

    fn room_with_canonical_alias(
        room_alias_id: &RoomAliasId,
        user_id: &UserId,
    ) -> v4::SlidingSyncRoom {
        let mut room = v4::SlidingSyncRoom::new();

        let mut canonical_alias_event_content = RoomCanonicalAliasEventContent::new();
        canonical_alias_event_content.alias = Some(room_alias_id.to_owned());

        room.required_state.push(make_state_event(
            user_id,
            "",
            canonical_alias_event_content,
            None,
        ));

        room
    }

    fn room_with_timeline(events: &[serde_json::Value]) -> v4::SlidingSyncRoom {
        let mut room = v4::SlidingSyncRoom::new();
        room.timeline.extend(
            events
                .iter()
                .map(|e| Raw::from_json_string(e.to_string()).unwrap())
                .collect::<Vec<_>>(),
        );
        room
    }

    fn set_room_invited(room: &mut v4::SlidingSyncRoom, inviter: &UserId, invitee: &UserId) {
        // MSC3575 shows an almost-empty event to indicate that we are invited to a
        // room. Just the type is supplied.

        let evt = Raw::new(&json!({
            "type": "m.room.member",
        }))
        .expect("Failed to make raw event")
        .cast();

        room.invite_state = Some(vec![evt]);

        // We expect that there will also be an invite event in the required_state,
        // assuming you've asked for this type of event.
        room.required_state.push(make_state_event(
            inviter,
            invitee.as_str(),
            RoomMemberEventContent::new(MembershipState::Invite),
            None,
        ));
    }

    fn set_room_joined(room: &mut v4::SlidingSyncRoom, user_id: &UserId) {
        room.required_state.push(make_membership_event(user_id, MembershipState::Join));
    }

    fn set_room_left(room: &mut v4::SlidingSyncRoom, user_id: &UserId) {
        room.required_state.push(make_membership_event(user_id, MembershipState::Leave));
    }

    fn set_room_left_as_timeline_event(room: &mut v4::SlidingSyncRoom, user_id: &UserId) {
        room.timeline.push(make_membership_event(user_id, MembershipState::Leave));
    }

    fn make_membership_event<K>(user_id: &UserId, state: MembershipState) -> Raw<K> {
        make_state_event(user_id, user_id.as_str(), RoomMemberEventContent::new(state), None)
    }

    fn make_global_account_data_event<C: GlobalAccountDataEventContent, E>(content: C) -> Raw<E> {
        Raw::new(&json!({
            "type": content.event_type(),
            "content": content,
        }))
        .expect("Failed to create account data event")
        .cast()
    }

    fn make_state_event<C: StateEventContent, E>(
        sender: &UserId,
        state_key: &str,
        content: C,
        prev_content: Option<C>,
    ) -> Raw<E> {
        let unsigned = if let Some(prev_content) = prev_content {
            json!({ "prev_content": prev_content })
        } else {
            json!({})
        };

        Raw::new(&json!({
            "type": content.event_type(),
            "state_key": state_key,
            "content": content,
            "event_id": event_id!("$evt"),
            "sender": sender,
            "origin_server_ts": 10,
            "unsigned": unsigned,
        }))
        .expect("Failed to create state event")
        .cast()
    }
}
