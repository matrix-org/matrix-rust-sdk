#[cfg(feature = "e2e-encryption")]
use std::ops::Deref;

use ruma::api::client::sync::sync_events::{
    v3::{self, Ephemeral},
    v4,
};
#[cfg(feature = "e2e-encryption")]
use ruma::UserId;
use tracing::{debug, info, instrument};

use super::BaseClient;
use crate::{
    deserialized_responses::AmbiguityChanges,
    error::Result,
    rooms::RoomType,
    store::{ambiguity_map::AmbiguityCache, StateChanges},
    sync::{JoinedRoom, Rooms, SyncResponse},
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

        let v4::Extensions { to_device, e2ee, account_data, .. } = extensions;

        let to_device_events = to_device.as_ref().map(|v4| v4.events.clone()).unwrap_or_default();

        // Destructure the single `None` of the E2EE extension into separate objects
        // since that's what the `OlmMachine` API expects. Passing in the default
        // empty maps and vecs for this is completely fine, since the `OlmMachine`
        // assumes empty maps/vecs mean no change in the one-time key counts.

        // We declare default values that can be referenced hereinbelow. When we try to
        // extract values from `e2ee`, that would be unfortunate to clone the
        // value just to pass them (to remove them `e2ee`) as a reference later.
        let device_one_time_keys_count = Default::default();
        let device_unused_fallback_key_types = Default::default();

        let (device_lists, device_one_time_keys_count, device_unused_fallback_key_types) = e2ee
            .as_ref()
            .map(|e2ee| {
                (
                    e2ee.device_lists.clone(),
                    &e2ee.device_one_time_keys_count,
                    &e2ee.device_unused_fallback_key_types,
                )
            })
            .unwrap_or_else(|| {
                (Default::default(), &device_one_time_keys_count, &device_unused_fallback_key_types)
            });

        info!(
            to_device_events = to_device_events.len(),
            device_one_time_keys_count = device_one_time_keys_count.len(),
            device_unused_fallback_key_types =
                device_unused_fallback_key_types.as_ref().map(|v| v.len())
        );

        // Process the to-device events and other related e2ee data. This returns a list
        // of all the to-device events that were passed in but encrypted ones
        // were replaced with their decrypted version.
        #[cfg(feature = "e2e-encryption")]
        let to_device_events = {
            self.preprocess_to_device_events(
                to_device_events,
                &device_lists,
                device_one_time_keys_count,
                device_unused_fallback_key_types.as_deref(),
            )
            .await?
        };

        let store = self.store.clone();
        let mut changes = StateChanges::default();
        let mut ambiguity_cache = AmbiguityCache::new(store.inner.clone());

        if let Some(global_data) = account_data.as_ref() {
            self.handle_account_data(&global_data.global, &mut changes).await;
        }

        let push_rules = self.get_push_rules(&changes).await?;

        let mut new_rooms = Rooms::default();

        for (room_id, room_data) in rooms {
            if !room_data.invite_state.is_empty() {
                let invite_states = &room_data.invite_state;
                let room = store.get_or_create_stripped_room(room_id).await;
                let mut room_info = room.clone_info();
                room_info.mark_state_partially_synced();

                if let Some(r) = store.get_room(room_id) {
                    let mut room_info = r.clone_info();
                    room_info.mark_as_invited(); // FIXME: this might not be accurate
                    room_info.mark_state_partially_synced();
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
                room_info.mark_state_partially_synced();

                // FIXME not yet supported by sliding sync.
                // room_info.update_summary(&room_data.summary);

                room_info.set_prev_batch(room_data.prev_batch.as_deref());

                let mut user_ids = if !room_data.required_state.is_empty() {
                    self.handle_state(
                        &room_data.required_state,
                        &mut room_info,
                        &mut changes,
                        &mut ambiguity_cache,
                    )
                    .await?
                } else {
                    Default::default()
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

                let room_account_data = if let Some(inner_account_data) = &account_data {
                    if let Some(events) = inner_account_data.rooms.get(room_id) {
                        self.handle_room_account_data(room_id, events, &mut changes).await;
                        Some(events.to_vec())
                    } else {
                        None
                    }
                } else {
                    None
                };

                if room_data.limited {
                    room_info.mark_members_missing();
                }

                let timeline = self
                    .handle_timeline(
                        &room,
                        room_data.limited,
                        room_data.timeline.clone(),
                        room_data.prev_batch.clone(),
                        &push_rules,
                        &mut user_ids,
                        &mut room_info,
                        &mut changes,
                        &mut ambiguity_cache,
                    )
                    .await?;

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
                            o.update_tracked_users(user_ids).await?
                        }

                        if !user_ids.is_empty() {
                            o.update_tracked_users(user_ids.iter().map(Deref::deref)).await?;
                        }
                    }
                }
                let notification_count = room_data.unread_notifications.clone().into();
                room_info.update_notification_count(notification_count);

                new_rooms.join.insert(
                    room_id.clone(),
                    JoinedRoom::new(
                        timeline,
                        v3::State::with_events(room_data.required_state.clone()),
                        room_account_data.unwrap_or_default(),
                        Ephemeral::default(),
                        notification_count,
                    ),
                );

                changes.add_room(room_info);
            }
        }

        // TODO remove this, we're processing account data events here again
        // because we want to have the push rules in place before we process
        // rooms and their events, but we want to create the rooms before we
        // process the `m.direct` account data event.
        if let Some(global_data) = account_data.as_ref() {
            self.handle_account_data(&global_data.global, &mut changes).await;
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
            device_one_time_keys_count.iter().map(|(k, v)| (k.clone(), (*v).into())).collect();

        Ok(SyncResponse {
            rooms: new_rooms,
            ambiguity_changes: AmbiguityChanges { changes: ambiguity_cache.changes },
            notifications: changes.notifications,
            // FIXME not yet supported by sliding sync.
            presence: Default::default(),
            account_data: account_data.as_ref().map(|a| a.global.clone()).unwrap_or_default(),
            to_device_events,
            device_lists,
            device_one_time_keys_count,
        })
    }
}
