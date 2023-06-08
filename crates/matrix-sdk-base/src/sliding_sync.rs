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

#[cfg(feature = "e2e-encryption")]
use std::ops::Deref;

use ruma::{
    api::client::sync::sync_events::{
        v3::{self, InvitedRoom, RoomSummary},
        v4::{self, AccountData},
    },
    RoomId,
};
use tracing::{debug, info, instrument};

use super::BaseClient;
#[cfg(feature = "e2e-encryption")]
use crate::RoomMemberships;
use crate::{
    deserialized_responses::AmbiguityChanges,
    error::Result,
    rooms::RoomState,
    store::{ambiguity_map::AmbiguityCache, StateChanges, Store},
    sync::{JoinedRoom, Rooms, SyncResponse},
    Room, RoomInfo,
};

impl BaseClient {
    /// Process a response from a sliding sync call.
    ///
    /// # Arguments
    ///
    /// * `response` - The response that we received after a successful sliding
    ///   sync.
    #[instrument(skip_all, level = "trace")]
    pub async fn process_sliding_sync(&self, response: &v4::Response) -> Result<SyncResponse> {
        #[allow(unused_variables)]
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

        info!(rooms = rooms.len(), lists = lists.len(), extensions = !extensions.is_empty());

        if rooms.is_empty() && extensions.is_empty() {
            // we received a room reshuffling event only, there won't be anything for us to
            // process. stop early
            return Ok(SyncResponse::default());
        };

        let v4::Extensions { to_device, e2ee, account_data, receipts, .. } = extensions;

        let to_device = to_device.as_ref().map(|v4| v4.events.clone()).unwrap_or_default();

        info!(
            to_device_events = to_device.len(),
            device_one_time_keys_count = e2ee.device_one_time_keys_count.len(),
            device_unused_fallback_key_types =
                e2ee.device_unused_fallback_key_types.as_ref().map(|v| v.len())
        );

        // Process the to-device events and other related e2ee data. This returns a list
        // of all the to-device events that were passed in but encrypted ones
        // were replaced with their decrypted version.
        // Passing in the default empty maps and vecs for this is completely fine, since
        // the `OlmMachine` assumes empty maps/vecs mean no change in the one-time key
        // counts.
        #[cfg(feature = "e2e-encryption")]
        let to_device = self
            .preprocess_to_device_events(
                to_device,
                &e2ee.device_lists,
                &e2ee.device_one_time_keys_count,
                e2ee.device_unused_fallback_key_types.as_deref(),
            )
            .await?;

        let store = self.store.clone();
        let mut changes = StateChanges::default();
        let mut ambiguity_cache = AmbiguityCache::new(store.inner.clone());

        if !account_data.is_empty() {
            self.handle_account_data(&account_data.global, &mut changes).await;
        }

        let mut new_rooms = Rooms::default();

        for (room_id, room_data) in rooms {
            let (room_to_store, joined_room, invited_room) = self
                .process_sliding_sync_room(
                    room_id,
                    room_data,
                    &store,
                    &mut changes,
                    &mut ambiguity_cache,
                    account_data,
                )
                .await?;
            changes.add_room(room_to_store);
            if let Some(joined_room) = joined_room {
                new_rooms.join.insert(room_id.clone(), joined_room);
            }
            if let Some(invited_room) = invited_room {
                new_rooms.invite.insert(room_id.clone(), invited_room);
            }
        }

        // Process receipts now we have rooms
        for (room_id, raw) in &receipts.rooms {
            match raw.deserialize() {
                Ok(event) => {
                    changes.add_receipts(room_id, event.content);
                }
                Err(e) => {
                    let event_id: Option<String> = raw.get_field("event_id").ok().flatten();
                    #[rustfmt::skip]
                    info!(
                        ?room_id, event_id,
                        "Failed to deserialize ephemeral room event: {e}"
                    );
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

        debug!("ready to submit changes to store");

        store.save_changes(&changes).await?;
        self.apply_changes(&changes).await;
        debug!("applied changes");

        let device_one_time_keys_count =
            e2ee.device_one_time_keys_count.iter().map(|(k, v)| (k.clone(), (*v).into())).collect();

        Ok(SyncResponse {
            rooms: new_rooms,
            ambiguity_changes: AmbiguityChanges { changes: ambiguity_cache.changes },
            notifications: changes.notifications,
            // FIXME not yet supported by sliding sync.
            presence: Default::default(),
            account_data: account_data.global.clone(),
            to_device,
            device_lists: e2ee.device_lists.clone(),
            device_one_time_keys_count,
        })
    }

    async fn process_sliding_sync_room(
        &self,
        room_id: &RoomId,
        room_data: &v4::SlidingSyncRoom,
        store: &Store,
        changes: &mut StateChanges,
        ambiguity_cache: &mut AmbiguityCache,
        account_data: &AccountData,
    ) -> Result<(RoomInfo, Option<JoinedRoom>, Option<InvitedRoom>)> {
        // Find or create the room in the store
        let (room, mut room_info, invited_room) =
            self.process_sliding_sync_room_membership(room_data, store, room_id, changes).await;

        room_info.mark_state_partially_synced();

        let mut user_ids = if !room_data.required_state.is_empty() {
            self.handle_state(&room_data.required_state, &mut room_info, changes, ambiguity_cache)
                .await?
        } else {
            Default::default()
        };

        let room_account_data = if let Some(events) = account_data.rooms.get(room_id) {
            self.handle_room_account_data(room_id, events, changes).await;
            Some(events.to_vec())
        } else {
            None
        };

        process_room_properties(room_data, &mut room_info);

        let push_rules = self.get_push_rules(changes).await?;

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
                ambiguity_cache,
            )
            .await?;

        #[cfg(feature = "e2e-encryption")]
        if room_info.is_encrypted() {
            if let Some(o) = self.olm_machine() {
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

        // If this room was not an invite, we treat it as joined
        // FIXME: it could be left, or possibly some other state
        let joined_room = if invited_room.is_none() {
            Some(JoinedRoom::new(
                timeline,
                room_data.required_state.clone(),
                room_account_data.unwrap_or_default(),
                Vec::new(),
                notification_count,
            ))
        } else {
            None
        };

        Ok((room_info, joined_room, invited_room))
    }

    /// Look through the sliding sync data for this room, find/create it in the
    /// store, and process any invite information.
    /// If any invite_state exists, we take it to mean that we are invited to
    /// this room https://github.com/matrix-org/matrix-spec-proposals/blob/kegan/sync-v3/proposals/3575-sync.md#room-list-parameters
    async fn process_sliding_sync_room_membership(
        &self,
        room_data: &v4::SlidingSyncRoom,
        store: &Store,
        room_id: &RoomId,
        changes: &mut StateChanges,
    ) -> (Room, RoomInfo, Option<InvitedRoom>) {
        if let Some(invite_state) = &room_data.invite_state {
            let room = store.get_or_create_stripped_room(room_id).await;
            let mut room_info = room.clone_info();

            // We don't actually know what events are inside invite_state. In theory, they
            // might not contain an m.room.member event, or they might set the
            // membership to something other than invite. This would be very
            // weird behaviour by the server, because invite_state is supposed
            // to contain an m.room.member. Later, we will call
            // handle_invited_state, which will reflect any information found in
            // the real events inside invite_state, but we default to considering this room
            // invited simply because invite_state exists. This is needed in the normal
            // case, because the sliding sync server tries to send minimal
            // state, meaning that we normally actually just receive {"type":
            // "m.room.member"} with no content at all.
            room_info.mark_as_invited();

            self.handle_invited_state(invite_state.as_slice(), &mut room_info, changes);

            (
                room,
                room_info,
                Some(v3::InvitedRoom::from(v3::InviteState::from(invite_state.clone()))),
            )
        } else {
            let room = store.get_or_create_room(room_id, RoomState::Joined).await;
            let mut room_info = room.clone_info();

            // We default to considering this room joined if it's not an invite, but we
            // expect to find a m.room.member event in the required_state, so
            // this should get fixed to the real correct value when we call
            // self.handle_state below.
            room_info.mark_as_joined();

            (room, room_info, None)
        }
    }
}

fn process_room_properties(room_data: &v4::SlidingSyncRoom, room_info: &mut RoomInfo) {
    if let Some(name) = &room_data.name {
        room_info.update_name(name.to_owned());
    }

    // Sliding sync doesn't have a room summary, nevertheless it contains the joined
    // and invited member counts. It likely will never have a heroes concept since
    // it calculates the room display name for us.
    //
    // Let's at least fetch the member counts, since they might be useful.
    let mut room_summary = RoomSummary::new();
    room_summary.invited_member_count = room_data.invited_count;
    room_summary.joined_member_count = room_data.joined_count;
    room_info.update_summary(&room_summary);

    room_info.set_prev_batch(room_data.prev_batch.as_deref());

    if room_data.limited {
        room_info.mark_members_missing();
    }
}

#[cfg(test)]
mod test {
    use ruma::{
        device_id, event_id,
        events::{
            room::{
                avatar::RoomAvatarEventContent, canonical_alias::RoomCanonicalAliasEventContent,
            },
            StateEventContent,
        },
        mxc_uri, room_alias_id, room_id,
        serde::Raw,
        uint, user_id, MxcUri, RoomAliasId, RoomId, UserId,
    };
    use serde_json::json;

    use super::*;
    use crate::SessionMeta;

    #[tokio::test]
    async fn can_process_empty_sliding_sync_response() {
        let client = logged_in_client().await;
        let empty_response = v4::Response::new("5".to_owned());
        client.process_sliding_sync(&empty_response).await.expect("Failed to process sync");
    }

    #[tokio::test]
    async fn room_with_unspecified_state_is_added_to_client_and_joined_list() {
        // Given a logged-in client
        let client = logged_in_client().await;
        let room_id = room_id!("!r:e.uk");

        // When I send sliding sync response containing a room (with identifiable data
        // in joined_count)
        let mut room = v4::SlidingSyncRoom::new();
        room.joined_count = Some(uint!(41));
        let response = response_with_room(room_id, room).await;
        let sync_resp =
            client.process_sliding_sync(&response).await.expect("Failed to process sync");

        // Then the room appears in the client (with the same joined count)
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(client_room.room_id(), room_id);
        assert_eq!(client_room.joined_members_count(), 41);
        assert_eq!(client_room.state(), RoomState::Joined);

        // And it is added to the list of joined rooms, and not the invited ones
        assert!(sync_resp.rooms.join.get(room_id).is_some());
        assert!(sync_resp.rooms.invite.get(room_id).is_none());
    }

    #[tokio::test]
    async fn room_name_is_found_when_processing_sliding_sync_response() {
        // Given a logged-in client
        let client = logged_in_client().await;
        let room_id = room_id!("!r:e.uk");

        // When I send sliding sync response containing a room with a name
        let mut room = v4::SlidingSyncRoom::new();
        room.name = Some("little room".to_owned());
        let response = response_with_room(room_id, room).await;
        client.process_sliding_sync(&response).await.expect("Failed to process sync");

        // Then the room appears in the client with the expected name
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(client_room.name(), Some("little room".to_owned()));
    }

    #[tokio::test]
    async fn invited_room_name_is_found_when_processing_sliding_sync_response() {
        // Given a logged-in client
        let client = logged_in_client().await;
        let room_id = room_id!("!r:e.uk");

        // When I send sliding sync response containing a room with a name
        let mut room = v4::SlidingSyncRoom::new();
        set_room_invited(&mut room);
        room.name = Some("little room".to_owned());
        let response = response_with_room(room_id, room).await;
        client.process_sliding_sync(&response).await.expect("Failed to process sync");

        // Then the room appears in the client with the expected name
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(client_room.name(), Some("little room".to_owned()));
    }

    #[tokio::test]
    async fn avatar_is_found_when_processing_sliding_sync_response() {
        // Given a logged-in client
        let client = logged_in_client().await;
        let room_id = room_id!("!r:e.uk");
        let user_id = user_id!("@u:e.uk");

        // When I send sliding sync response containing a room with an avatar
        let room = room_with_avatar(mxc_uri!("mxc://e.uk/med1"), user_id);
        let response = response_with_room(room_id, room).await;
        client.process_sliding_sync(&response).await.expect("Failed to process sync");

        // Then the room in the client has the avatar
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(
            client_room.avatar_url().expect("No avatar URL").media_id().expect("No media ID"),
            "med1"
        );
    }

    #[tokio::test]
    async fn invitation_room_is_added_to_client_and_invite_list() {
        // Given a logged-in client
        let client = logged_in_client().await;
        let room_id = room_id!("!r:e.uk");

        // When I send sliding sync response containing an invited room
        let mut room = v4::SlidingSyncRoom::new();
        set_room_invited(&mut room);
        let response = response_with_room(room_id, room).await;
        let sync_resp =
            client.process_sliding_sync(&response).await.expect("Failed to process sync");

        // Then the room is added to the client
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(client_room.room_id(), room_id);
        assert_eq!(client_room.state(), RoomState::Invited);

        // And it is added to the list of invited rooms, not the joined ones
        assert!(!sync_resp.rooms.invite[room_id].invite_state.is_empty());
        assert!(sync_resp.rooms.join.get(room_id).is_none());
    }

    #[tokio::test]
    async fn avatar_is_found_in_invitation_room_when_processing_sliding_sync_response() {
        // Given a logged-in client
        let client = logged_in_client().await;
        let room_id = room_id!("!r:e.uk");
        let user_id = user_id!("@u:e.uk");

        // When I send sliding sync response containing an invited room with an avatar
        let mut room = room_with_avatar(mxc_uri!("mxc://e.uk/med1"), user_id);
        set_room_invited(&mut room);
        let response = response_with_room(room_id, room).await;
        client.process_sliding_sync(&response).await.expect("Failed to process sync");

        // Then the room in the client has the avatar
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(
            client_room.avatar_url().expect("No avatar URL").media_id().expect("No media ID"),
            "med1"
        );
    }

    #[tokio::test]
    async fn canonical_alias_is_found_in_invitation_room_when_processing_sliding_sync_response() {
        // Given a logged-in client
        let client = logged_in_client().await;
        let room_id = room_id!("!r:e.uk");
        let user_id = user_id!("@u:e.uk");
        let room_alias_id = room_alias_id!("#myroom:e.uk");

        // When I send sliding sync response containing an invited room with an avatar
        let mut room = room_with_canonical_alias(room_alias_id, user_id);
        set_room_invited(&mut room);
        let response = response_with_room(room_id, room).await;
        client.process_sliding_sync(&response).await.expect("Failed to process sync");

        // Then the room in the client has the avatar
        let client_room = client.get_room(room_id).expect("No room found");
        assert_eq!(client_room.canonical_alias(), Some(room_alias_id.to_owned()));
    }

    async fn logged_in_client() -> BaseClient {
        let client = BaseClient::new();
        client
            .set_session_meta(SessionMeta {
                user_id: user_id!("@u:e.uk").to_owned(),
                device_id: device_id!("XYZ").to_owned(),
            })
            .await
            .expect("Failed to set session meta");
        client
    }

    async fn response_with_room(room_id: &RoomId, room: v4::SlidingSyncRoom) -> v4::Response {
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

    fn set_room_invited(room: &mut v4::SlidingSyncRoom) {
        // MSC3575 shows an almost-empty event to indicate that we are invited to a
        // room. Just the type is supplied.

        let evt = Raw::new(&json!({
            "type": "m.room.member",
        }))
        .expect("Failed to make raw event")
        .cast();

        room.invite_state = Some(vec![evt]);
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
