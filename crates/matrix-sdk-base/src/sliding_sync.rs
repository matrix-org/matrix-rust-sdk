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

//! Extend `BaseClient` with capabilities to handle MSC4186.

#[cfg(feature = "e2e-encryption")]
use matrix_sdk_common::deserialized_responses::ProcessedToDeviceEvent;
use matrix_sdk_common::{deserialized_responses::TimelineEvent, timer};
use ruma::{
    OwnedRoomId, api::client::sync::sync_events::v5 as http, events::receipt::SyncReceiptEvent,
    serde::Raw,
};
use tracing::{instrument, trace};

use super::BaseClient;
use crate::{
    RequestedRequiredStates,
    error::Result,
    read_receipts::compute_unread_counts,
    response_processors as processors,
    room::RoomInfoNotableUpdateReasons,
    store::ambiguity_map::AmbiguityCache,
    sync::{RoomUpdates, SyncResponse},
};

impl BaseClient {
    /// Processes the E2EE-related events from the Sliding Sync response.
    ///
    /// In addition to writes to the crypto store, this may also write into the
    /// state store, in particular it may write latest-events to the state
    /// store.
    ///
    /// Returns whether any change happened.
    #[cfg(feature = "e2e-encryption")]
    pub async fn process_sliding_sync_e2ee(
        &self,
        to_device: Option<&http::response::ToDevice>,
        e2ee: &http::response::E2EE,
    ) -> Result<Option<Vec<ProcessedToDeviceEvent>>> {
        if to_device.is_none() && e2ee.is_empty() {
            return Ok(None);
        }

        trace!(
            to_device_events =
                to_device.map(|to_device| to_device.events.len()).unwrap_or_default(),
            device_one_time_keys_count = e2ee.device_one_time_keys_count.len(),
            device_unused_fallback_key_types =
                e2ee.device_unused_fallback_key_types.as_ref().map(|v| v.len()),
            "Processing sliding sync e2ee events",
        );

        let olm_machine = self.olm_machine().await;

        let context = processors::Context::default();

        let processors::e2ee::to_device::Output { processed_to_device_events } =
            processors::e2ee::to_device::from_msc4186(
                to_device,
                e2ee,
                olm_machine.as_ref(),
                &self.decryption_settings,
            )
            .await?;

        processors::changes::save_and_apply(
            context,
            &self.state_store,
            &self.ignore_user_list_changes,
            None,
        )
        .await?;

        Ok(Some(processed_to_device_events))
    }

    /// Process a response from a sliding sync call.
    ///
    /// # Arguments
    ///
    /// * `response` - The response that we received after a successful sliding
    ///   sync.
    #[instrument(skip_all, level = "trace")]
    pub async fn process_sliding_sync(
        &self,
        response: &http::Response,
        requested_required_states: &RequestedRequiredStates,
    ) -> Result<SyncResponse> {
        let http::Response { rooms, lists, extensions, .. } = response;

        trace!(
            rooms = rooms.len(),
            lists = lists.len(),
            has_extensions = !extensions.is_empty(),
            "Processing sliding sync room events"
        );

        if rooms.is_empty() && extensions.is_empty() {
            // we received a room reshuffling event only, there won't be anything for us to
            // process. stop early
            return Ok(SyncResponse::default());
        }

        let _timer = timer!(tracing::Level::TRACE, "_method");

        let mut context = processors::Context::default();

        let state_store = self.state_store.clone();
        let mut ambiguity_cache = AmbiguityCache::new(state_store.inner.clone());

        let global_account_data_processor =
            processors::account_data::global(&extensions.account_data.global);
        let push_rules = self.get_push_rules(&global_account_data_processor).await?;

        let mut room_updates = RoomUpdates::default();
        let mut notifications = Default::default();

        let user_id = self
            .session_meta()
            .expect("Sliding sync shouldn't run without an authenticated user.")
            .user_id
            .to_owned();

        for (room_id, room_response) in rooms {
            let Some((room_info, room_update)) = processors::room::msc4186::update_any_room(
                &mut context,
                &user_id,
                processors::room::RoomCreationData::new(
                    room_id,
                    self.room_info_notable_update_sender.clone(),
                    requested_required_states,
                    &mut ambiguity_cache,
                ),
                room_response,
                &extensions.account_data.rooms,
                #[cfg(feature = "e2e-encryption")]
                processors::e2ee::E2EE::new(
                    self.olm_machine().await.as_ref(),
                    &self.decryption_settings,
                    self.handle_verification_events,
                ),
                processors::notification::Notification::new(
                    &push_rules,
                    &mut notifications,
                    &self.state_store,
                ),
            )
            .await?
            else {
                continue;
            };

            context.state_changes.add_room(room_info);

            let room_id = room_id.to_owned();

            use processors::room::msc4186::RoomUpdateKind;

            match room_update {
                RoomUpdateKind::Joined(joined_room_update) => {
                    room_updates.joined.insert(room_id, joined_room_update);
                }
                RoomUpdateKind::Left(left_room_update) => {
                    room_updates.left.insert(room_id, left_room_update);
                }
                RoomUpdateKind::Invited(invited_room_update) => {
                    room_updates.invited.insert(room_id, invited_room_update);
                }
                RoomUpdateKind::Knocked(knocked_room_update) => {
                    room_updates.knocked.insert(room_id, knocked_room_update);
                }
            }
        }

        // Handle read receipts and typing notifications independently of the rooms:
        // these both live in a different subsection of the server's response,
        // so they may exist without any update for the associated room.
        processors::room::msc4186::extensions::dispatch_typing_ephemeral_events(
            &extensions.typing,
            &mut room_updates.joined,
        );

        // Handle room account data.
        processors::room::msc4186::extensions::room_account_data(
            &mut context,
            &extensions.account_data,
            &mut room_updates,
            &self.state_store,
        );

        global_account_data_processor.apply(&mut context, &state_store).await;

        context.state_changes.ambiguity_maps = ambiguity_cache.cache;

        // Save the changes and apply them.
        processors::changes::save_and_apply(
            context,
            &self.state_store,
            &self.ignore_user_list_changes,
            None,
        )
        .await?;

        let mut context = processors::Context::default();

        // Now that all the rooms information have been saved, update the display name
        // of the updated rooms (which relies on information stored in the database).
        processors::room::display_name::update_for_rooms(
            &mut context,
            &room_updates,
            &self.state_store,
        )
        .await;

        // Save the new display name updates if any.
        processors::changes::save_only(context, &self.state_store).await?;

        Ok(SyncResponse {
            rooms: room_updates,
            notifications,
            presence: Default::default(),
            account_data: extensions.account_data.global.clone(),
            to_device: Default::default(),
        })
    }

    /// Process the `receipts` extension, and compute (and save) the unread
    /// counts based on read receipts, for a particular room.
    #[doc(hidden)]
    pub async fn process_sliding_sync_receipts_extension_for_room(
        &self,
        room_id: &OwnedRoomId,
        response: &http::Response,
        new_sync_events: Vec<TimelineEvent>,
        room_previous_events: Vec<TimelineEvent>,
    ) -> Result<Option<Raw<SyncReceiptEvent>>> {
        let mut context = processors::Context::default();

        let mut save_context = false;

        // Handle the receipt ephemeral event.
        let receipt_ephemeral_event = if let Some(receipt_ephemeral_event) =
            response.extensions.receipts.rooms.get(room_id)
        {
            processors::room::msc4186::extensions::dispatch_receipt_ephemeral_event_for_room(
                &mut context,
                room_id,
                receipt_ephemeral_event,
            );
            save_context = true;
            Some(receipt_ephemeral_event.clone())
        } else {
            None
        };

        let user_id = &self.session_meta().expect("logged in user").user_id;

        // Rooms in `room_updates.joined` either have a timeline update, or a new read
        // receipt. Update the read receipt accordingly.
        if let Some(mut room_info) = self.get_room(room_id).map(|room| room.clone_info()) {
            let prev_read_receipts = room_info.read_receipts.clone();

            compute_unread_counts(
                user_id,
                room_id,
                context.state_changes.receipts.get(room_id),
                room_previous_events,
                &new_sync_events,
                &mut room_info.read_receipts,
                self.threading_support,
            );

            if prev_read_receipts != room_info.read_receipts {
                context
                    .room_info_notable_updates
                    .entry(room_id.clone())
                    .or_default()
                    .insert(RoomInfoNotableUpdateReasons::READ_RECEIPT);

                context.state_changes.add_room(room_info);
                save_context = true;
            }
        }

        // Save the new `RoomInfo` if updated.
        if save_context {
            processors::changes::save_only(context, &self.state_store).await?;
        }

        Ok(receipt_ephemeral_event)
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::collections::{BTreeMap, HashSet};

    use assert_matches::assert_matches;
    use matrix_sdk_test::async_test;
    use ruma::{
        JsOption, MxcUri, OwnedRoomId, OwnedUserId, RoomAliasId, RoomId, UserId,
        api::client::sync::sync_events::UnreadNotificationsCount,
        assign, event_id,
        events::{
            GlobalAccountDataEventContent, StateEventContent, StateEventType,
            direct::{DirectEventContent, DirectUserIdentifier, OwnedDirectUserIdentifier},
            room::{
                avatar::RoomAvatarEventContent,
                canonical_alias::RoomCanonicalAliasEventContent,
                encryption::RoomEncryptionEventContent,
                member::{MembershipState, RoomMemberEventContent},
                name::RoomNameEventContent,
                pinned_events::RoomPinnedEventsEventContent,
            },
        },
        mxc_uri, owned_event_id, owned_mxc_uri, owned_user_id, room_alias_id, room_id,
        serde::Raw,
        uint, user_id,
    };
    use serde_json::json;

    use super::http;
    use crate::{
        BaseClient, EncryptionState, RequestedRequiredStates, RoomInfoNotableUpdate, RoomState,
        SessionMeta,
        client::ThreadingSupport,
        room::{RoomHero, RoomInfoNotableUpdateReasons},
        store::{RoomLoadSettings, StoreConfig},
        test_utils::logged_in_base_client,
    };

    #[async_test]
    async fn test_notification_count_set() {
        let client = logged_in_base_client(None).await;

        let mut response = http::Response::new("42".to_owned());
        let room_id = room_id!("!room:example.org");
        let count = assign!(UnreadNotificationsCount::default(), {
            highlight_count: Some(uint!(13)),
            notification_count: Some(uint!(37)),
        });

        response.rooms.insert(
            room_id.to_owned(),
            assign!(http::response::Room::new(), {
                unread_notifications: count.clone()
            }),
        );

        let sync_response = client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // Check it's present in the response.
        let room = sync_response.rooms.joined.get(room_id).unwrap();
        assert_eq!(room.unread_notifications, count.clone().into());

        // Check it's been updated in the store.
        let room = client.get_room(room_id).expect("found room");
        assert_eq!(room.unread_notification_counts(), count.into());
    }

    #[async_test]
    async fn test_can_process_empty_sliding_sync_response() {
        let client = logged_in_base_client(None).await;
        let empty_response = http::Response::new("5".to_owned());
        client
            .process_sliding_sync(&empty_response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");
    }

    #[async_test]
    async fn test_room_with_unspecified_state_is_added_to_client_and_joined_list() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");

        // When I send sliding sync response containing a room (with identifiable data
        // in joined_count)
        let mut room = http::response::Room::new();
        room.joined_count = Some(uint!(41));
        let response = response_with_room(room_id, room);
        let sync_resp = client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // Then the room appears in the client (with the same joined count)
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(client_room.room_id(), room_id);
        assert_eq!(client_room.joined_members_count(), 41);
        assert_eq!(client_room.state(), RoomState::Joined);

        // And it is added to the list of joined rooms only.
        assert!(sync_resp.rooms.joined.contains_key(room_id));
        assert!(!sync_resp.rooms.left.contains_key(room_id));
        assert!(!sync_resp.rooms.invited.contains_key(room_id));
    }

    #[async_test]
    async fn test_missing_room_name_event() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");

        // When I send sliding sync response containing a room with a name set in the
        // sliding sync response,
        let mut room = http::response::Room::new();
        room.name = Some("little room".to_owned());
        let response = response_with_room(room_id, room);
        let sync_resp = client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // No m.room.name event, no heroes, no members => considered an empty room!
        let client_room = client.get_room(room_id).expect("No room found");
        assert!(client_room.name().is_none());
        assert_eq!(
            client_room.compute_display_name().await.unwrap().into_inner().to_string(),
            "Empty Room"
        );
        assert_eq!(client_room.state(), RoomState::Joined);

        // And it is added to the list of joined rooms only.
        assert!(sync_resp.rooms.joined.contains_key(room_id));
        assert!(!sync_resp.rooms.left.contains_key(room_id));
        assert!(!sync_resp.rooms.invited.contains_key(room_id));
        assert!(!sync_resp.rooms.knocked.contains_key(room_id));
    }

    #[async_test]
    async fn test_room_name_event() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");

        // When I send sliding sync response containing a room with a name set in the
        // sliding sync response, and a m.room.name event,
        let mut room = http::response::Room::new();

        room.name = Some("little room".to_owned());
        set_room_name(&mut room, user_id!("@a:b.c"), "The Name".to_owned());

        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // The name is known.
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(client_room.name().as_deref(), Some("The Name"));
        assert_eq!(
            client_room.compute_display_name().await.unwrap().into_inner().to_string(),
            "The Name"
        );
    }

    #[async_test]
    async fn test_missing_invited_room_name_event() {
        // Given a logged-in client,
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");
        let user_id = user_id!("@w:e.uk");
        let inviter = user_id!("@john:mastodon.org");

        // When I send sliding sync response containing a room with a name set in the
        // sliding sync response,
        let mut room = http::response::Room::new();
        set_room_invited(&mut room, inviter, user_id);
        room.name = Some("name from sliding sync response".to_owned());
        let response = response_with_room(room_id, room);
        let sync_resp = client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // Then the room doesn't have the name in the client.
        let client_room = client.get_room(room_id).expect("No room found");
        assert!(client_room.name().is_none());

        // No m.room.name event, no heroes => using the invited member.
        assert_eq!(client_room.compute_display_name().await.unwrap().into_inner().to_string(), "w");

        assert_eq!(client_room.state(), RoomState::Invited);

        // And it is added to the list of invited rooms only.
        assert!(!sync_resp.rooms.joined.contains_key(room_id));
        assert!(!sync_resp.rooms.left.contains_key(room_id));
        assert!(sync_resp.rooms.invited.contains_key(room_id));
        assert!(!sync_resp.rooms.knocked.contains_key(room_id));
    }

    #[async_test]
    async fn test_invited_room_name_event() {
        // Given a logged-in client,
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");
        let user_id = user_id!("@w:e.uk");
        let inviter = user_id!("@john:mastodon.org");

        // When I send sliding sync response containing a room with a name set in the
        // sliding sync response, and a m.room.name event,
        let mut room = http::response::Room::new();

        set_room_invited(&mut room, inviter, user_id);

        room.name = Some("name from sliding sync response".to_owned());
        set_room_name(&mut room, user_id!("@a:b.c"), "The Name".to_owned());

        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // The name is known.
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(client_room.name().as_deref(), Some("The Name"));
        assert_eq!(
            client_room.compute_display_name().await.unwrap().into_inner().to_string(),
            "The Name"
        );
    }

    #[async_test]
    async fn test_receiving_a_knocked_room_membership_event_creates_a_knocked_room() {
        // Given a logged-in client,
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");
        let user_id = client.session_meta().unwrap().user_id.to_owned();

        // When the room is properly set as knocked with the current user id as state
        // key,
        let mut room = http::response::Room::new();
        set_room_knocked(&mut room, &user_id);

        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // The room is knocked.
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(client_room.state(), RoomState::Knocked);
    }

    #[async_test]
    async fn test_receiving_a_knocked_room_membership_event_with_wrong_state_key_creates_an_invited_room()
     {
        // Given a logged-in client,
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");
        let user_id = user_id!("@w:e.uk");

        // When the room is set as knocked with a random user id as state key,
        let mut room = http::response::Room::new();
        set_room_knocked(&mut room, user_id);

        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // The room is invited since the membership event doesn't belong to the current
        // user.
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(client_room.state(), RoomState::Invited);
    }

    #[async_test]
    async fn test_receiving_an_unknown_room_membership_event_in_invite_state_creates_an_invited_room()
     {
        // Given a logged-in client,
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");
        let user_id = client.session_meta().unwrap().user_id.to_owned();

        // When the room has the wrong membership state in its invite_state
        let mut room = http::response::Room::new();
        let event = Raw::new(&json!({
            "type": "m.room.member",
            "sender": user_id,
            "content": {
                "is_direct": true,
                "membership": "join",
            },
            "state_key": user_id,
        }))
        .expect("Failed to make raw event")
        .cast_unchecked();
        room.invite_state = Some(vec![event]);

        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // The room is marked as invited.
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(client_room.state(), RoomState::Invited);
    }

    #[async_test]
    async fn test_left_a_room_from_required_state_event() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");
        let user_id = user_id!("@u:e.uk");

        // When I join…
        let mut room = http::response::Room::new();
        set_room_joined(&mut room, user_id);
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");
        assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Joined);

        // And then leave with a `required_state` state event…
        let mut room = http::response::Room::new();
        set_room_left(&mut room, user_id);
        let response = response_with_room(room_id, room);
        let sync_resp = client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // The room is left.
        assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Left);

        // And it is added to the list of left rooms only.
        assert!(!sync_resp.rooms.joined.contains_key(room_id));
        assert!(sync_resp.rooms.left.contains_key(room_id));
        assert!(!sync_resp.rooms.invited.contains_key(room_id));
        assert!(!sync_resp.rooms.knocked.contains_key(room_id));
    }

    #[async_test]
    async fn test_kick_or_ban_updates_room_to_left() {
        for membership in [MembershipState::Leave, MembershipState::Ban] {
            let room_id = room_id!("!r:e.uk");
            let user_a_id = user_id!("@a:e.uk");
            let user_b_id = user_id!("@b:e.uk");
            let client = logged_in_base_client(Some(user_a_id)).await;

            // When I join…
            let mut room = http::response::Room::new();
            set_room_joined(&mut room, user_a_id);
            let response = response_with_room(room_id, room);
            client
                .process_sliding_sync(&response, &RequestedRequiredStates::default())
                .await
                .expect("Failed to process sync");
            assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Joined);

            // And then get kicked/banned with a `required_state` state event…
            let mut room = http::response::Room::new();
            room.required_state.push(make_state_event(
                user_b_id,
                user_a_id.as_str(),
                RoomMemberEventContent::new(membership.clone()),
                None,
            ));
            let response = response_with_room(room_id, room);
            let sync_resp = client
                .process_sliding_sync(&response, &RequestedRequiredStates::default())
                .await
                .expect("Failed to process sync");

            match membership {
                MembershipState::Leave => {
                    // The room is left.
                    assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Left);
                }
                MembershipState::Ban => {
                    // The room is banned.
                    assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Banned);
                }
                _ => panic!("Unexpected membership state found: {membership}"),
            }

            // And it is added to the list of left rooms only.
            assert!(!sync_resp.rooms.joined.contains_key(room_id));
            assert!(sync_resp.rooms.left.contains_key(room_id));
            assert!(!sync_resp.rooms.invited.contains_key(room_id));
            assert!(!sync_resp.rooms.knocked.contains_key(room_id));
        }
    }

    #[async_test]
    async fn test_left_a_room_from_timeline_state_event() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");
        let user_id = user_id!("@u:e.uk");

        // When I join…
        let mut room = http::response::Room::new();
        set_room_joined(&mut room, user_id);
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");
        assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Joined);

        // And then leave with a `timeline` state event…
        let mut room = http::response::Room::new();
        set_room_left_as_timeline_event(&mut room, user_id);
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // The room is NOT left because state events from `timeline` must be IGNORED!
        assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Joined);
    }

    #[async_test]
    async fn test_can_be_reinvited_to_a_left_room() {
        // See https://github.com/matrix-org/matrix-rust-sdk/issues/1834

        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");
        let user_id = user_id!("@u:e.uk");

        // When I join...
        let mut room = http::response::Room::new();
        set_room_joined(&mut room, user_id);
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");
        // (sanity: state is join)
        assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Joined);

        // And then leave...
        let mut room = http::response::Room::new();
        set_room_left(&mut room, user_id);
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");
        // (sanity: state is left)
        assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Left);

        // And then get invited back
        let mut room = http::response::Room::new();
        set_room_invited(&mut room, user_id, user_id);
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

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
        assert!(
            direct_targets(&client, room_id).contains(<&DirectUserIdentifier>::from(user_b_id))
        );
        assert_eq!(membership(&client, room_id, user_b_id).await, MembershipState::Join);

        // When B leaves
        update_room_membership(&client, room_id, user_b_id, MembershipState::Leave).await;

        // Then B is still a direct target, and is in Leave state (B is a direct target
        // because we want to return to our old DM in the UI even if the other
        // user left, so we can reinvite them. See https://github.com/matrix-org/matrix-rust-sdk/issues/2017)
        assert!(
            direct_targets(&client, room_id).contains(<&DirectUserIdentifier>::from(user_b_id))
        );
        assert_eq!(membership(&client, room_id, user_b_id).await, MembershipState::Leave);
    }

    #[async_test]
    async fn test_other_person_refusing_invite_to_a_dm_is_reflected_in_their_membership_and_direct_targets()
     {
        let room_id = room_id!("!r:e.uk");
        let user_a_id = user_id!("@a:e.uk");
        let user_b_id = user_id!("@b:e.uk");

        // Given I have invited B to a DM
        let client = logged_in_base_client(None).await;
        create_dm(&client, room_id, user_a_id, user_b_id, MembershipState::Invite).await;

        // (Sanity: B is a direct target, and is in Invite state)
        assert!(
            direct_targets(&client, room_id).contains(<&DirectUserIdentifier>::from(user_b_id))
        );
        assert_eq!(membership(&client, room_id, user_b_id).await, MembershipState::Invite);

        // When B declines the invitation (i.e. leaves)
        update_room_membership(&client, room_id, user_b_id, MembershipState::Leave).await;

        // Then B is still a direct target, and is in Leave state (B is a direct target
        // because we want to return to our old DM in the UI even if the other
        // user left, so we can reinvite them. See https://github.com/matrix-org/matrix-rust-sdk/issues/2017)
        assert!(
            direct_targets(&client, room_id).contains(<&DirectUserIdentifier>::from(user_b_id))
        );
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
        assert!(
            direct_targets(&client, room_id).contains(<&DirectUserIdentifier>::from(user_b_id))
        );
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
        assert!(
            direct_targets(&client, room_id).contains(<&DirectUserIdentifier>::from(user_b_id))
        );
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
            let mut room = http::response::Room::new();
            room.avatar = JsOption::from_option(Some(mxc_uri!("mxc://e.uk/med1").to_owned()));

            room
        };
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

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
            let mut room = http::response::Room::new();
            room.avatar = JsOption::from_option(Some(mxc_uri!("mxc://e.uk/med1").to_owned()));

            room
        };
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // Then the room in the client has the avatar
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(
            client_room.avatar_url().expect("No avatar URL").media_id().expect("No media ID"),
            "med1"
        );

        // No avatar. Still here.

        // When I send sliding sync response containing no avatar.
        let room = http::response::Room::new();
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // Then the room in the client still has the avatar
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(
            client_room.avatar_url().expect("No avatar URL").media_id().expect("No media ID"),
            "med1"
        );

        // Avatar is unset.

        // When I send sliding sync response containing an avatar set to `null` (!).
        let room = {
            let mut room = http::response::Room::new();
            room.avatar = JsOption::Null;

            room
        };
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

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
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

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
        let mut room = http::response::Room::new();
        set_room_invited(&mut room, user_id, user_id);
        let response = response_with_room(room_id, room);
        let sync_resp = client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // Then the room is added to the client
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(client_room.room_id(), room_id);
        assert_eq!(client_room.state(), RoomState::Invited);

        // And it is added to the list of invited rooms, not the joined ones
        assert!(!sync_resp.rooms.invited[room_id].invite_state.is_empty());
        assert!(!sync_resp.rooms.joined.contains_key(room_id));
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
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // Then the room in the client has the avatar
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(
            client_room.avatar_url().expect("No avatar URL").media_id().expect("No media ID"),
            "med1"
        );
    }

    #[async_test]
    async fn test_canonical_alias_is_found_in_invitation_room_when_processing_sliding_sync_response()
     {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");
        let user_id = user_id!("@u:e.uk");
        let room_alias_id = room_alias_id!("#myroom:e.uk");

        // When I send sliding sync response containing an invited room with an avatar
        let mut room = room_with_canonical_alias(room_alias_id, user_id);
        set_room_invited(&mut room, user_id, user_id);
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

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
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // Then the room's name is NOT overridden by the server-computed display name.
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(
            client_room.compute_display_name().await.unwrap().into_inner().to_string(),
            "myroom"
        );
        assert!(client_room.name().is_none());
    }

    #[async_test]
    async fn test_display_name_is_cached_and_emits_a_notable_update_reason() {
        let client = logged_in_base_client(None).await;
        let user_id = user_id!("@u:e.uk");
        let room_id = room_id!("!r:e.uk");

        let mut room_info_notable_update = client.room_info_notable_update_receiver();

        let room = room_with_name("Hello World", user_id);
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        let room = client.get_room(room_id).expect("No room found");
        assert_eq!(room.cached_display_name().unwrap().to_string(), "Hello World");

        assert_matches!(
            room_info_notable_update.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(reasons.contains(RoomInfoNotableUpdateReasons::NONE));
            }
        );
        assert_matches!(
            room_info_notable_update.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons }) => {
                assert_eq!(received_room_id, room_id);
                // The reason we are looking for :-].
                assert!(reasons.contains(RoomInfoNotableUpdateReasons::DISPLAY_NAME));
            }
        );
        assert!(room_info_notable_update.is_empty());
    }

    #[async_test]
    async fn test_display_name_is_persisted_from_sliding_sync() {
        let user_id = user_id!("@u:e.uk");
        let room_id = room_id!("!r:e.uk");
        let session_meta = SessionMeta { user_id: user_id.to_owned(), device_id: "FOOBAR".into() };
        let state_store;

        {
            let client = {
                let store = StoreConfig::new("cross-process-foo".to_owned());
                state_store = store.state_store.clone();

                let client = BaseClient::new(store, ThreadingSupport::Disabled);
                client
                    .activate(
                        session_meta.clone(),
                        RoomLoadSettings::default(),
                        #[cfg(feature = "e2e-encryption")]
                        None,
                    )
                    .await
                    .expect("`activate` failed!");

                client
            };

            // When the sliding sync response contains an explicit room name as well as an
            // alias
            let room = room_with_name("Hello World", user_id);
            let response = response_with_room(room_id, room);
            client
                .process_sliding_sync(&response, &RequestedRequiredStates::default())
                .await
                .expect("Failed to process sync");

            let room = client.get_room(room_id).expect("No room found");
            assert_eq!(room.cached_display_name().unwrap().to_string(), "Hello World");
        }

        {
            let client = {
                let mut store = StoreConfig::new("cross-process-foo".to_owned());
                store.state_store = state_store;
                let client = BaseClient::new(store, ThreadingSupport::Disabled);
                client
                    .activate(
                        session_meta,
                        RoomLoadSettings::default(),
                        #[cfg(feature = "e2e-encryption")]
                        None,
                    )
                    .await
                    .expect("`activate` failed!");

                client
            };

            let room = client.get_room(room_id).expect("No room found");
            assert_eq!(room.cached_display_name().unwrap().to_string(), "Hello World");
        }
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
        let mut room = http::response::Room::new();
        room.heroes = Some(vec![
            assign!(http::response::Hero::new(gordon), {
                name: Some("Gordon".to_owned()),
            }),
            assign!(http::response::Hero::new(alice), {
                name: Some("Alice".to_owned()),
                avatar: Some(owned_mxc_uri!("mxc://e.uk/med1"))
            }),
        ]);
        let response = response_with_room(room_id, room);
        let _sync_resp = client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // Then the room appears in the client.
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(client_room.room_id(), room_id);
        assert_eq!(client_room.state(), RoomState::Joined);

        // And heroes are part of the summary.
        assert_eq!(
            client_room.clone_info().summary.heroes(),
            &[
                RoomHero {
                    user_id: owned_user_id!("@gordon:e.uk"),
                    display_name: Some("Gordon".to_owned()),
                    avatar_url: None
                },
                RoomHero {
                    user_id: owned_user_id!("@alice:e.uk"),
                    display_name: Some("Alice".to_owned()),
                    avatar_url: Some(owned_mxc_uri!("mxc://e.uk/med1"))
                },
            ]
        );
    }

    #[async_test]
    async fn test_recency_stamp_is_found_when_processing_sliding_sync_response() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");

        // When I send sliding sync response containing a room with a recency stamp
        let room = assign!(http::response::Room::new(), {
            bump_stamp: Some(42u32.into()),
        });
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // Then the room in the client has the recency stamp
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(client_room.recency_stamp().expect("No recency stamp"), 42.into());
    }

    #[async_test]
    async fn test_recency_stamp_can_be_overwritten_when_present_in_a_sliding_sync_response() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let room_id = room_id!("!r:e.uk");

        {
            // When I send sliding sync response containing a room with a recency stamp
            let room = assign!(http::response::Room::new(), {
                bump_stamp: Some(42u32.into()),
            });
            let response = response_with_room(room_id, room);
            client
                .process_sliding_sync(&response, &RequestedRequiredStates::default())
                .await
                .expect("Failed to process sync");

            // Then the room in the client has the recency stamp
            let client_room = client.get_room(room_id).expect("No room found");
            assert_eq!(client_room.recency_stamp().expect("No recency stamp"), 42.into());
        }

        {
            // When I send sliding sync response containing a room with NO recency stamp
            let room = assign!(http::response::Room::new(), {
                bump_stamp: None,
            });
            let response = response_with_room(room_id, room);
            client
                .process_sliding_sync(&response, &RequestedRequiredStates::default())
                .await
                .expect("Failed to process sync");

            // Then the room in the client has the previous recency stamp
            let client_room = client.get_room(room_id).expect("No room found");
            assert_eq!(client_room.recency_stamp().expect("No recency stamp"), 42.into());
        }

        {
            // When I send sliding sync response containing a room with a NEW recency
            // timestamp
            let room = assign!(http::response::Room::new(), {
                bump_stamp: Some(153u32.into()),
            });
            let response = response_with_room(room_id, room);
            client
                .process_sliding_sync(&response, &RequestedRequiredStates::default())
                .await
                .expect("Failed to process sync");

            // Then the room in the client has the recency stamp
            let client_room = client.get_room(room_id).expect("No room found");
            assert_eq!(client_room.recency_stamp().expect("No recency stamp"), 153.into());
        }
    }

    #[async_test]
    async fn test_recency_stamp_can_trigger_a_notable_update_reason() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let mut room_info_notable_update_stream = client.room_info_notable_update_receiver();
        let room_id = room_id!("!r:e.uk");

        // When I send sliding sync response containing a room with a recency stamp.
        let room = assign!(http::response::Room::new(), {
            bump_stamp: Some(42u32.into()),
        });
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // Then a room info notable update is NOT received, because it's the first time
        // the room is seen.
        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(!received_reasons.contains(RoomInfoNotableUpdateReasons::RECENCY_STAMP));
            }
        );
        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(received_reasons.contains(RoomInfoNotableUpdateReasons::DISPLAY_NAME));
            }
        );
        assert!(room_info_notable_update_stream.is_empty());

        // When I send sliding sync response containing a room with a recency stamp.
        let room = assign!(http::response::Room::new(), {
            bump_stamp: Some(43u32.into()),
        });
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // Then a room info notable update is received.
        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(received_reasons.contains(RoomInfoNotableUpdateReasons::RECENCY_STAMP));
            }
        );
        assert!(room_info_notable_update_stream.is_empty());
    }

    #[async_test]
    async fn test_leaving_room_can_trigger_a_notable_update_reason() {
        // Given a logged-in client
        let client = logged_in_base_client(None).await;
        let mut room_info_notable_update_stream = client.room_info_notable_update_receiver();

        // When I send sliding sync response containing a new room.
        let room_id = room_id!("!r:e.uk");
        let room = http::response::Room::new();
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // Other notable update reason. We don't really care about them here.
        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(received_reasons.contains(RoomInfoNotableUpdateReasons::NONE));
            }
        );
        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(received_reasons.contains(RoomInfoNotableUpdateReasons::DISPLAY_NAME));
            }
        );

        // Send sliding sync response containing a membership event with 'join' value.
        let room_id = room_id!("!r:e.uk");
        let events = vec![
            Raw::from_json_string(
                json!({
                    "type": "m.room.member",
                    "event_id": "$3",
                    "content": { "membership": "join" },
                    "sender": "@u:h.uk",
                    "origin_server_ts": 12344445,
                    "state_key": "@u:e.uk",
                })
                .to_string(),
            )
            .unwrap(),
        ];
        let room = assign!(http::response::Room::new(), {
            required_state: events,
        });
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // Room was already joined, no `MEMBERSHIP` update should be triggered here
        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(received_reasons.contains(RoomInfoNotableUpdateReasons::NONE));
            }
        );
        assert!(room_info_notable_update_stream.is_empty());

        let events = vec![
            Raw::from_json_string(
                json!({
                    "type": "m.room.member",
                    "event_id": "$3",
                    "content": { "membership": "leave" },
                    "sender": "@u:h.uk",
                    "origin_server_ts": 12344445,
                    "state_key": "@u:e.uk",
                })
                .to_string(),
            )
            .unwrap(),
        ];
        let room = assign!(http::response::Room::new(), {
            required_state: events,
        });
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // Then a room info notable update is received.
        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(received_reasons.contains(RoomInfoNotableUpdateReasons::MEMBERSHIP));
            }
        );
        assert!(room_info_notable_update_stream.is_empty());
    }

    #[async_test]
    async fn test_unread_marker_can_trigger_a_notable_update_reason() {
        // Given a logged-in client,
        let client = logged_in_base_client(None).await;
        let mut room_info_notable_update_stream = client.room_info_notable_update_receiver();

        // When I receive a sliding sync response containing a new room,
        let room_id = room_id!("!r:e.uk");
        let room = http::response::Room::new();
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // Other notable updates are received, but not the ones we are interested by.
        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(received_reasons.contains(RoomInfoNotableUpdateReasons::NONE), "{received_reasons:?}");
            }
        );
        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(received_reasons.contains(RoomInfoNotableUpdateReasons::DISPLAY_NAME), "{received_reasons:?}");
            }
        );
        assert!(room_info_notable_update_stream.is_empty());

        // When I receive a sliding sync response containing one update about an unread
        // marker,
        let room_id = room_id!("!r:e.uk");
        let room_account_data_events = vec![
            Raw::from_json_string(
                json!({
                    "type": "m.marked_unread",
                    "event_id": "$1",
                    "content": { "unread": true },
                    "sender": client.session_meta().unwrap().user_id,
                    "origin_server_ts": 12344445,
                })
                .to_string(),
            )
            .unwrap(),
        ];
        let mut response = response_with_room(room_id, http::response::Room::new());
        response.extensions.account_data.rooms.insert(room_id.to_owned(), room_account_data_events);

        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // Then a room info notable update is received.
        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(received_reasons.contains(RoomInfoNotableUpdateReasons::UNREAD_MARKER), "{received_reasons:?}");
            }
        );

        // But getting it again won't trigger a new notable update…
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(received_reasons.contains(RoomInfoNotableUpdateReasons::NONE), "{received_reasons:?}");
            }
        );
        assert!(room_info_notable_update_stream.is_empty());

        // …Unless its value changes!
        let room_account_data_events = vec![
            Raw::from_json_string(
                json!({
                    "type": "m.marked_unread",
                    "event_id": "$1",
                    "content": { "unread": false },
                    "sender": client.session_meta().unwrap().user_id,
                    "origin_server_ts": 12344445,
                })
                .to_string(),
            )
            .unwrap(),
        ];
        response.extensions.account_data.rooms.insert(room_id.to_owned(), room_account_data_events);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(received_reasons.contains(RoomInfoNotableUpdateReasons::UNREAD_MARKER));
            }
        );
        assert!(room_info_notable_update_stream.is_empty());
    }

    #[async_test]
    async fn test_unstable_unread_marker_is_ignored_after_stable() {
        // Given a logged-in client,
        let client = logged_in_base_client(None).await;
        let mut room_info_notable_update_stream = client.room_info_notable_update_receiver();

        // When I receive a sliding sync response containing a new room,
        let room_id = room_id!("!r:e.uk");
        let room = http::response::Room::new();
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // Other notable updates are received, but not the ones we are interested by.
        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(received_reasons.contains(RoomInfoNotableUpdateReasons::NONE), "{received_reasons:?}");
            }
        );
        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(received_reasons.contains(RoomInfoNotableUpdateReasons::DISPLAY_NAME), "{received_reasons:?}");
            }
        );
        assert!(room_info_notable_update_stream.is_empty());

        // When I receive a sliding sync response containing one update about an
        // unstable unread marker,
        let room_id = room_id!("!r:e.uk");
        let unstable_room_account_data_events = vec![
            Raw::from_json_string(
                json!({
                    "type": "com.famedly.marked_unread",
                    "event_id": "$1",
                    "content": { "unread": true },
                    "sender": client.session_meta().unwrap().user_id,
                    "origin_server_ts": 12344445,
                })
                .to_string(),
            )
            .unwrap(),
        ];
        let mut response = response_with_room(room_id, http::response::Room::new());
        response
            .extensions
            .account_data
            .rooms
            .insert(room_id.to_owned(), unstable_room_account_data_events.clone());

        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // Then a room info notable update is received.
        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(received_reasons.contains(RoomInfoNotableUpdateReasons::UNREAD_MARKER), "{received_reasons:?}");
            }
        );
        assert!(room_info_notable_update_stream.is_empty());

        // When I receive a sliding sync response with a stable unread marker update,
        let stable_room_account_data_events = vec![
            Raw::from_json_string(
                json!({
                    "type": "m.marked_unread",
                    "event_id": "$1",
                    "content": { "unread": false },
                    "sender": client.session_meta().unwrap().user_id,
                    "origin_server_ts": 12344445,
                })
                .to_string(),
            )
            .unwrap(),
        ];
        response
            .extensions
            .account_data
            .rooms
            .insert(room_id.to_owned(), stable_room_account_data_events);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // Then a room info notable update is received.
        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(received_reasons.contains(RoomInfoNotableUpdateReasons::UNREAD_MARKER));
            }
        );
        assert!(room_info_notable_update_stream.is_empty());

        // When I receive a sliding sync response with an unstable unread
        // marker update again,
        response
            .extensions
            .account_data
            .rooms
            .insert(room_id.to_owned(), unstable_room_account_data_events);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // There is no notable update.
        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(received_reasons.contains(RoomInfoNotableUpdateReasons::NONE), "{received_reasons:?}");
            }
        );
        assert!(room_info_notable_update_stream.is_empty());

        // Finally, when I receive a sliding sync response with a stable unread marker
        // update again,
        let stable_room_account_data_events = vec![
            Raw::from_json_string(
                json!({
                    "type": "m.marked_unread",
                    "event_id": "$3",
                    "content": { "unread": true },
                    "sender": client.session_meta().unwrap().user_id,
                    "origin_server_ts": 12344445,
                })
                .to_string(),
            )
            .unwrap(),
        ];
        response
            .extensions
            .account_data
            .rooms
            .insert(room_id.to_owned(), stable_room_account_data_events);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // Then a room info notable update is received.
        assert_matches!(
            room_info_notable_update_stream.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons: received_reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(received_reasons.contains(RoomInfoNotableUpdateReasons::UNREAD_MARKER));
            }
        );
        assert!(room_info_notable_update_stream.is_empty());
    }

    #[async_test]
    async fn test_pinned_events_are_updated_on_sync() {
        let user_a_id = user_id!("@a:e.uk");
        let client = logged_in_base_client(Some(user_a_id)).await;
        let room_id = room_id!("!r:e.uk");
        let pinned_event_id = owned_event_id!("$an-id:e.uk");

        // Create room
        let mut room_response = http::response::Room::new();
        set_room_joined(&mut room_response, user_a_id);
        let response = response_with_room(room_id, room_response);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        // The newly created room has no pinned event ids
        let room = client.get_room(room_id).unwrap();
        let pinned_event_ids = room.pinned_event_ids();
        assert_matches!(pinned_event_ids, None);

        // Load new pinned event id
        let mut room_response = http::response::Room::new();
        room_response.required_state.push(make_state_event(
            user_a_id,
            "",
            RoomPinnedEventsEventContent::new(vec![pinned_event_id.clone()]),
            None,
        ));
        let response = response_with_room(room_id, room_response);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        let pinned_event_ids = room.pinned_event_ids().unwrap_or_default();
        assert_eq!(pinned_event_ids.len(), 1);
        assert_eq!(pinned_event_ids[0], pinned_event_id);

        // Pinned event ids are now empty
        let mut room_response = http::response::Room::new();
        room_response.required_state.push(make_state_event(
            user_a_id,
            "",
            RoomPinnedEventsEventContent::new(Vec::new()),
            None,
        ));
        let response = response_with_room(room_id, room_response);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");
        let pinned_event_ids = room.pinned_event_ids().unwrap();
        assert!(pinned_event_ids.is_empty());
    }

    #[async_test]
    async fn test_dms_are_processed_in_any_sync_response() {
        let current_user_id = user_id!("@current:e.uk");
        let client = logged_in_base_client(Some(current_user_id)).await;
        let user_a_id = user_id!("@a:e.uk");
        let user_b_id = user_id!("@b:e.uk");
        let room_id_1 = room_id!("!r:e.uk");
        let room_id_2 = room_id!("!s:e.uk");

        let mut room_response = http::response::Room::new();
        set_room_joined(&mut room_response, user_a_id);
        let mut response = response_with_room(room_id_1, room_response);
        let mut direct_content: BTreeMap<OwnedDirectUserIdentifier, Vec<OwnedRoomId>> =
            BTreeMap::new();
        direct_content.insert(user_a_id.into(), vec![room_id_1.to_owned()]);
        direct_content.insert(user_b_id.into(), vec![room_id_2.to_owned()]);
        response
            .extensions
            .account_data
            .global
            .push(make_global_account_data_event(DirectEventContent(direct_content)));
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        let room_1 = client.get_room(room_id_1).unwrap();
        assert!(room_1.is_direct().await.unwrap());

        // Now perform a sync without new account data
        let mut room_response = http::response::Room::new();
        set_room_joined(&mut room_response, user_b_id);
        let response = response_with_room(room_id_2, room_response);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");

        let room_2 = client.get_room(room_id_2).unwrap();
        assert!(room_2.is_direct().await.unwrap());
    }

    #[async_test]
    async fn test_room_encryption_state_is_and_is_not_encrypted() {
        let user_id = user_id!("@raclette:patate");
        let client = logged_in_base_client(Some(user_id)).await;
        let room_id_0 = room_id!("!r0");
        let room_id_1 = room_id!("!r1");
        let room_id_2 = room_id!("!r2");

        // A room is considered encrypted when it receives a `m.room.encryption` event,
        // period.
        //
        // A room is considered **not** encrypted when it receives no
        // `m.room.encryption` event but it was requested, period.
        //
        // We are going to test three rooms:
        //
        // - two of them receive a `m.room.encryption` event
        // - the last one does not receive a `m.room.encryption`.
        // - the first one is configured with a `required_state` for this event, the
        //   others have nothing.
        //
        // The trick is that, since sliding sync makes an union of all the
        // `required_state`s, then all rooms are technically requesting a
        // `m.room.encryption`.
        let requested_required_states = RequestedRequiredStates::from(&{
            let mut request = http::Request::new();

            request.room_subscriptions.insert(room_id_0.to_owned(), {
                let mut room_subscription = http::request::RoomSubscription::default();

                room_subscription
                    .required_state
                    .push((StateEventType::RoomEncryption, "".to_owned()));

                room_subscription
            });

            request
        });

        let mut response = http::Response::new("0".to_owned());

        // Create two rooms that are encrypted, i.e. they have a `m.room.encryption`
        // state event in their `required_state`. Create a third room that is not
        // encrypted, i.e. it doesn't have a `m.room.encryption` state event.
        {
            let not_encrypted_room = http::response::Room::new();
            let mut encrypted_room = http::response::Room::new();
            set_room_is_encrypted(&mut encrypted_room, user_id);

            response.rooms.insert(room_id_0.to_owned(), encrypted_room.clone());
            response.rooms.insert(room_id_1.to_owned(), encrypted_room);
            response.rooms.insert(room_id_2.to_owned(), not_encrypted_room);
        }

        client
            .process_sliding_sync(&response, &requested_required_states)
            .await
            .expect("Failed to process sync");

        // They are both encrypted, yepee.
        assert_matches!(
            client.get_room(room_id_0).unwrap().encryption_state(),
            EncryptionState::Encrypted
        );
        assert_matches!(
            client.get_room(room_id_1).unwrap().encryption_state(),
            EncryptionState::Encrypted
        );
        // This one is not encrypted because it has received nothing.
        assert_matches!(
            client.get_room(room_id_2).unwrap().encryption_state(),
            EncryptionState::NotEncrypted
        )
    }

    #[async_test]
    async fn test_room_encryption_state_is_unknown() {
        let user_id = user_id!("@raclette:patate");
        let client = logged_in_base_client(Some(user_id)).await;
        let room_id_0 = room_id!("!r0");
        let room_id_1 = room_id!("!r1");

        // A room is considered encrypted when it receives a `m.room.encryption` event,
        // period.
        //
        // A room is considered **not** encrypted when it receives no
        // `m.room.encryption` event but it was requested, period.
        //
        // We are going to test two rooms:
        //
        // - one that receives a `m.room.encryption` event,
        // - one that receives nothing,
        // - none of them have requested the state event.

        let requested_required_states = RequestedRequiredStates::from(&http::Request::new());

        let mut response = http::Response::new("0".to_owned());

        // Create two rooms with and without a `m.room.encryption` event.
        {
            let not_encrypted_room = http::response::Room::new();
            let mut encrypted_room = http::response::Room::new();
            set_room_is_encrypted(&mut encrypted_room, user_id);

            response.rooms.insert(room_id_0.to_owned(), encrypted_room);
            response.rooms.insert(room_id_1.to_owned(), not_encrypted_room);
        }

        client
            .process_sliding_sync(&response, &requested_required_states)
            .await
            .expect("Failed to process sync");

        // Encrypted, because the presence of a `m.room.encryption` always mean the room
        // is encrypted.
        assert_matches!(
            client.get_room(room_id_0).unwrap().encryption_state(),
            EncryptionState::Encrypted
        );
        // Unknown, because the absence of `m.room.encryption` when not requested
        // means we don't know what the state is.
        assert_matches!(
            client.get_room(room_id_1).unwrap().encryption_state(),
            EncryptionState::Unknown
        );
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

    fn direct_targets(client: &BaseClient, room_id: &RoomId) -> HashSet<OwnedDirectUserIdentifier> {
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
        let mut room = http::response::Room::new();
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
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");
    }

    /// Set this user's membership within this room to new_state
    async fn update_room_membership(
        client: &BaseClient,
        room_id: &RoomId,
        user_id: &UserId,
        new_state: MembershipState,
    ) {
        let mut room = http::response::Room::new();
        room.required_state.push(make_membership_event(user_id, new_state));
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync(&response, &RequestedRequiredStates::default())
            .await
            .expect("Failed to process sync");
    }

    fn set_direct_with(
        response: &mut http::Response,
        user_id: OwnedUserId,
        room_ids: Vec<OwnedRoomId>,
    ) {
        let mut direct_content: BTreeMap<OwnedDirectUserIdentifier, Vec<OwnedRoomId>> =
            BTreeMap::new();
        direct_content.insert(user_id.into(), room_ids);
        response
            .extensions
            .account_data
            .global
            .push(make_global_account_data_event(DirectEventContent(direct_content)));
    }

    fn response_with_room(room_id: &RoomId, room: http::response::Room) -> http::Response {
        let mut response = http::Response::new("5".to_owned());
        response.rooms.insert(room_id.to_owned(), room);
        response
    }

    fn room_with_avatar(avatar_uri: &MxcUri, user_id: &UserId) -> http::response::Room {
        let mut room = http::response::Room::new();

        let mut avatar_event_content = RoomAvatarEventContent::new();
        avatar_event_content.url = Some(avatar_uri.to_owned());

        room.required_state.push(make_state_event(user_id, "", avatar_event_content, None));

        room
    }

    fn room_with_canonical_alias(
        room_alias_id: &RoomAliasId,
        user_id: &UserId,
    ) -> http::response::Room {
        let mut room = http::response::Room::new();

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

    fn room_with_name(name: &str, user_id: &UserId) -> http::response::Room {
        let mut room = http::response::Room::new();

        let name_event_content = RoomNameEventContent::new(name.to_owned());

        room.required_state.push(make_state_event(user_id, "", name_event_content, None));

        room
    }

    fn set_room_name(room: &mut http::response::Room, sender: &UserId, name: String) {
        room.required_state.push(make_state_event(
            sender,
            "",
            RoomNameEventContent::new(name),
            None,
        ));
    }

    fn set_room_invited(room: &mut http::response::Room, inviter: &UserId, invitee: &UserId) {
        // Sliding Sync shows an almost-empty event to indicate that we are invited to a
        // room. Just the type is supplied.

        let evt = Raw::new(&json!({
            "type": "m.room.member",
            "sender": inviter,
            "content": {
                "is_direct": true,
                "membership": "invite",
            },
            "state_key": invitee,
        }))
        .expect("Failed to make raw event")
        .cast_unchecked();

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

    fn set_room_knocked(room: &mut http::response::Room, knocker: &UserId) {
        // Sliding Sync shows an almost-empty event to indicate that we are invited to a
        // room. Just the type is supplied.

        let evt = Raw::new(&json!({
            "type": "m.room.member",
            "sender": knocker,
            "content": {
                "is_direct": true,
                "membership": "knock",
            },
            "state_key": knocker,
        }))
        .expect("Failed to make raw event")
        .cast_unchecked();

        room.invite_state = Some(vec![evt]);
    }

    fn set_room_joined(room: &mut http::response::Room, user_id: &UserId) {
        room.required_state.push(make_membership_event(user_id, MembershipState::Join));
    }

    fn set_room_left(room: &mut http::response::Room, user_id: &UserId) {
        room.required_state.push(make_membership_event(user_id, MembershipState::Leave));
    }

    fn set_room_left_as_timeline_event(room: &mut http::response::Room, user_id: &UserId) {
        room.timeline.push(make_membership_event(user_id, MembershipState::Leave));
    }

    fn set_room_is_encrypted(room: &mut http::response::Room, user_id: &UserId) {
        room.required_state.push(make_encryption_event(user_id));
    }

    fn make_membership_event<K>(user_id: &UserId, state: MembershipState) -> Raw<K> {
        make_state_event(user_id, user_id.as_str(), RoomMemberEventContent::new(state), None)
    }

    fn make_encryption_event<K>(user_id: &UserId) -> Raw<K> {
        make_state_event(user_id, "", RoomEncryptionEventContent::with_recommended_defaults(), None)
    }

    fn make_global_account_data_event<C: GlobalAccountDataEventContent, E>(content: C) -> Raw<E> {
        Raw::new(&json!({
            "type": content.event_type(),
            "content": content,
        }))
        .expect("Failed to create account data event")
        .cast_unchecked()
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
        .cast_unchecked()
    }
}
