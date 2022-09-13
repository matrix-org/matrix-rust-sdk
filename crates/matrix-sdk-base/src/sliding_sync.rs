#[cfg(feature = "e2e-encryption")]
use std::ops::Deref;

use matrix_sdk_common::deserialized_responses::{
    AmbiguityChanges, JoinedRoom, Rooms, SyncResponse,
};
use ruma::api::client::sync::sync_events::{v3, v4};
#[cfg(feature = "e2e-encryption")]
use ruma::UserId;

use super::BaseClient;
use crate::{
    error::Result,
    rooms::RoomType,
    store::{ambiguity_map::AmbiguityCache, StateChanges},
};

impl BaseClient {
    /// Process a response from a sliding sync call.
    ///
    /// # Arguments
    ///
    /// * `response` - The response that we received after a successful sliding
    ///   sync.
    pub async fn process_sliding_sync(&self, response: v4::Response) -> Result<SyncResponse> {
        #[allow(unused_variables)]
        let v4::Response {
            // FIXME not yet supported by sliding sync. see
            // https://github.com/matrix-org/matrix-rust-sdk/issues/1014
            // next_batch,
            rooms,
            lists,
            // FIXME: missing compared to v3::Response
            //presence,
            //account_data,
            //to_device,
            //device_lists,
            //device_one_time_keys_count,
            //device_unused_fallback_key_types,
            ..
        } = response;

        // FIXME not yet supported by sliding sync. see
        // https://github.com/matrix-org/matrix-rust-sdk/issues/1014
        // #[cfg(feature = "encryption")]
        // let to_device = {
        //     if let Some(o) = self.olm_machine().await {
        //         // Let the crypto machine handle the sync response, this
        //         // decrypts to-device events, but leaves room events alone.
        //         // This makes sure that we have the decryption keys for the room
        //         // events at hand.
        //         o.receive_sync_changes(
        //             to_device,
        //             &device_lists,
        //             &device_one_time_keys_count,
        //             device_unused_fallback_key_types.as_deref(),
        //         )
        //         .await?
        //     } else {
        //         to_device
        //     }
        // };

        if rooms.is_empty() {
            // nothing for us to handle here
            return Ok(SyncResponse::default());
        };

        let store = self.store.clone();
        let mut changes = StateChanges::default();
        let mut ambiguity_cache = AmbiguityCache::new(store.inner.clone());

        // FIXME not yet supported by sliding sync.
        // self.handle_account_data(&account_data.events, &mut changes).await;

        let _push_rules = self.get_push_rules(&changes).await?;

        let mut new_rooms = Rooms::default();

        for (room_id, room_data) in &rooms {
            if !room_data.invite_state.is_empty() {
                let invite_states = &room_data.invite_state;
                let room = store.get_or_create_stripped_room(room_id).await;
                let mut room_info = room.clone_info();

                if let Some(r) = store.get_room(room_id) {
                    let mut room_info = r.clone_info();
                    room_info.mark_as_invited(); // FIXME: this might not be accurate
                    changes.add_room(room_info);
                }

                self.handle_invited_state(invite_states.as_slice(), &mut room_info, &mut changes);

                new_rooms.invite.insert(
                    room_id.clone(),
                    v3::InvitedRoom::from(v3::InviteState::from(invite_states.clone())),
                );
            } else {
                let room = store.get_or_create_room(room_id, RoomType::Joined).await;
                let mut room_info = room.clone_info();
                room_info.mark_as_joined(); // FIXME: this might not be accurate

                // FIXME not yet supported by sliding sync.
                // room_info.update_summary(&room_data.summary);

                room_info.set_prev_batch(room_data.prev_batch.as_deref());

                let user_ids = if room_data.required_state.is_empty() {
                    None
                } else {
                    Some(
                        self.handle_state(
                            &room_data.required_state,
                            &mut room_info,
                            &mut changes,
                            &mut ambiguity_cache,
                        )
                        .await?,
                    )
                };

                // FIXME not yet supported by sliding sync. see
                // https://github.com/matrix-org/matrix-rust-sdk/issues/1014
                // if let Some(event) =
                //     room_data.ephemeral.events.iter().find_map(|e| match e.deserialize() {
                //         Ok(AnySyncEphemeralRoomEvent::Receipt(event)) => Some(event.content),
                //         _ => None,
                //     })
                // {
                //     changes.add_receipts(&room_id, event);
                // }

                // FIXME not yet supported by sliding sync.
                // self.handle_room_account_data(&room_id, &room_data.account_data.events, &mut
                // changes)     .await;

                // FIXME not yet supported by sliding sync.
                // if room_data.timeline.limited {
                //     room_info.mark_members_missing();
                // }

                // let timeline = self
                //     .handle_timeline(
                //         &room,
                //         room_data.timeline,
                //         &push_rules,
                //         &mut room_info,
                //         &mut changes,
                //         &mut ambiguity_cache,
                //         &mut user_ids,
                //     )
                //     .await?;

                // let timeline_slice = TimelineSlice::new(
                //     timeline.events.clone(),
                //     next_batch.clone(),
                //     timeline.prev_batch.clone(),
                //     timeline.limited,
                //     true,
                // );

                // changes.add_timeline(&room_id, timeline_slice);

                #[cfg(feature = "e2e-encryption")]
                if room_info.is_encrypted() {
                    if let Some(o) = self.olm_machine() {
                        if !room.is_encrypted() {
                            // The room turned on encryption in this sync, we need
                            // to also get all the existing users and mark them for
                            // tracking.
                            let joined = store.get_joined_user_ids(room_id).await?;
                            let invited = store.get_invited_user_ids(room_id).await?;

                            let user_ids: Vec<&UserId> =
                                joined.iter().chain(&invited).map(Deref::deref).collect();
                            o.update_tracked_users(user_ids).await
                        }

                        if let Some(user_ids) = user_ids {
                            o.update_tracked_users(user_ids.iter().map(Deref::deref)).await;
                        }
                    }
                }
                let notification_count = room_data.unread_notifications.clone().into();
                room_info.update_notification_count(notification_count);

                new_rooms.join.insert(
                    room_id.clone(),
                    JoinedRoom::new(
                        Default::default(), //timeline,
                        v3::State::with_events(room_data.required_state.clone()),
                        Default::default(), // room_info.account_data,
                        Default::default(), // room_info.ephemeral,
                        notification_count,
                    ),
                );

                changes.add_room(room_info);
            }
        }

        // FIXME not yet supported by sliding sync. see
        // https://github.com/matrix-org/matrix-rust-sdk/issues/1014
        // self.handle_account_data(&account_data.events, &mut changes).await;

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

        tracing::debug!("ready to submit changes to store");

        store.save_changes(&changes).await?;
        self.apply_changes(&changes).await;
        tracing::debug!("applied changes");

        Ok(SyncResponse {
            next_batch: "test".into(),
            rooms: new_rooms,
            ambiguity_changes: AmbiguityChanges { changes: ambiguity_cache.changes },
            notifications: changes.notifications,
            // FIXME not yet supported by sliding sync.
            presence: Default::default(),
            account_data: Default::default(),
            to_device: Default::default(),
            device_lists: Default::default(),
            device_one_time_keys_count: Default::default(),
        })
    }
}
