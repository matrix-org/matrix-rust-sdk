use super::BaseClient;
use ruma::{
    api::client::sync::{sync_events::v3, sliding_sync_events},
    UserId, RoomId,
};
use std::ops::Deref;
use crate::{
    rooms::RoomType,
    error::Result,
    store::{
        ambiguity_map::AmbiguityCache, StateChanges, Store, StoreConfig,
    },
};

use matrix_sdk_common::{
    deserialized_responses::{
        AmbiguityChanges, JoinedRoom, LeftRoom, MemberEvent, MembersResponse, Rooms,
        StrippedMemberEvent, SyncResponse, SyncRoomEvent, Timeline, TimelineSlice,
    },
    instant::Instant,
    locks::RwLock,
    util::milli_seconds_since_unix_epoch,
};

impl BaseClient {

    pub async fn process_sliding_sync(
        &self,
        response: sliding_sync_events::Response,
    ) -> Result<SyncResponse> {
        #[allow(unused_variables)]
        let sliding_sync_events::Response {
            // not applicable.
            // next_batch,
            room_subscriptions,
            ops,
            // FIXME: missing compared to v3::Response
            //presence,
            //account_data,
            //to_device,
            //device_lists,
            //device_one_time_keys_count,
            //device_unused_fallback_key_types,
            ..
        } = response;
        let mut rooms = {
            if let Some(subs) = room_subscriptions {
                subs.clone()
            } else {
                Default::default()
            }
        };

        

        for op in ops
            .unwrap_or_default()
            .iter()
            .filter_map(|r| r.deserialize().ok())
        {
            let local_rooms = if let Some(rooms) = &op.rooms {
                rooms.clone()
            } else if let Some(room) = op.room {
                vec![room]
            } else {
                // op has no room updates, skipping
                continue
            };

            for room in local_rooms {
                let room_id = if let Some(Ok(room_id)) = room.room_id.clone().map(RoomId::parse) {
                    room_id
                } else {
                    continue
                };
                rooms
                    .entry(room_id)
                    .and_modify(|entry| {
                        // different ops might filter different fields, so
                        // we need to merge the data
                        for e in &room.timeline {
                            let event_id : String = if let Ok(Some(event_id)) = e.get_field("event_id") { event_id } else { continue };
                            if !entry.timeline.iter().find(|t|
                                t.get_field("event_id").ok().flatten().map(|f: String| f == event_id).unwrap_or_default()
                            ).is_none() {
                                entry.timeline.push(e.clone());
                            }
                        }
                        for e in &room.required_state {
                            let event_id: String = if let Ok(Some(event_id)) = e.get_field("event_id") { event_id } else { continue };
                            if !entry.required_state.iter().find(|t|
                                    t.get_field("event_id").ok().flatten().map(|f: String| f == event_id).unwrap_or_default()
                            ).is_none() {
                                entry.required_state.push(e.clone());
                            }
                        }

                        // we assume the other fields are the same for the same response answer
                    })
                    .or_insert_with(|| room.clone());
            }

        }

        // FIXME not yet supported by sliding sync. 
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

        let store = self.store();
        let mut changes = StateChanges::default();
        let mut ambiguity_cache = AmbiguityCache::new(store.clone());

        // FIXME not yet supported by sliding sync. 
        // self.handle_account_data(&account_data.events, &mut changes).await;
        
        let push_rules = self.get_push_rules(&changes).await?;

        let mut new_rooms = Rooms::default();

        for (room_id, room_data) in rooms.iter() {
            if let Some(invite_states) = &room_data.invite_state {
                let room = store.get_or_create_stripped_room(&room_id).await;
                let mut room_info = room.clone_info();
    
                if let Some(r) = store.get_room(&room_id) {
                    let mut room_info = r.clone_info();
                    room_info.mark_as_invited();
                    changes.add_room(room_info);
                }
    
                let (members, state_events) =
                    self.handle_invited_state(invite_states.as_slice(), &mut room_info);
    
                changes.stripped_members.insert(room_id.clone(), members);
                changes.stripped_state.insert(room_id.clone(), state_events);
                changes.add_stripped_room(room_info);

                new_rooms.invite.insert(room_id.clone(), v3::InvitedRoom::from(v3::InviteState::from(invite_states.clone())));
            } else {
                
                let room = store.get_or_create_room(&room_id, RoomType::Joined).await;
                let mut room_info = room.clone_info();
                room_info.mark_as_joined();


                // FIXME not yet supported by sliding sync. 
                // room_info.update_summary(&room_data.summary);

                room_info.set_prev_batch(room_data.prev_batch.as_deref());

                let mut user_ids = self
                    .handle_state(
                        &mut changes,
                        &mut ambiguity_cache,
                        &room_data.required_state,
                        &mut room_info,
                    )
                    .await?;


                // FIXME not yet supported by sliding sync. 
                // if let Some(event) =
                //     room_data.ephemeral.events.iter().find_map(|e| match e.deserialize() {
                //         Ok(AnySyncEphemeralRoomEvent::Receipt(event)) => Some(event.content),
                //         _ => None,
                //     })
                // {
                //     changes.add_receipts(&room_id, event);
                // }


                // FIXME not yet supported by sliding sync. 
                // self.handle_room_account_data(&room_id, &room_data.account_data.events, &mut changes)
                //     .await;

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

                #[cfg(feature = "encryption")]
                if room_info.is_encrypted() {
                    if let Some(o) = self.olm_machine().await {
                        if !room.is_encrypted() {
                            // The room turned on encryption in this sync, we need
                            // to also get all the existing users and mark them for
                            // tracking.
                            let joined = store.get_joined_user_ids(&room_id).await?;
                            let invited = store.get_invited_user_ids(&room_id).await?;

                            let user_ids: Vec<&UserId> =
                                joined.iter().chain(&invited).map(Deref::deref).collect();
                            o.update_tracked_users(user_ids).await
                        }

                        o.update_tracked_users(user_ids.iter().map(Deref::deref)).await;
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


        // FIXME not yet supported by sliding sync. 
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

        store.save_changes(&changes).await?;
        self.apply_changes(&changes).await;

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