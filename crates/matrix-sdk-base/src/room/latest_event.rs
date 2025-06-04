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

#[cfg(feature = "e2e-encryption")]
use std::{collections::BTreeMap, num::NonZeroUsize};

#[cfg(feature = "e2e-encryption")]
use ruma::{events::AnySyncTimelineEvent, serde::Raw, OwnedRoomId};

use super::Room;
#[cfg(feature = "e2e-encryption")]
use super::RoomInfoNotableUpdateReasons;
use crate::latest_event::LatestEvent;

impl Room {
    /// The size of the latest_encrypted_events RingBuffer
    #[cfg(feature = "e2e-encryption")]
    pub(super) const MAX_ENCRYPTED_EVENTS: NonZeroUsize = NonZeroUsize::new(10).unwrap();

    /// Return the last event in this room, if one has been cached during
    /// sliding sync.
    pub fn latest_event(&self) -> Option<LatestEvent> {
        self.inner.read().latest_event.as_deref().cloned()
    }

    /// Return the most recent few encrypted events. When the keys come through
    /// to decrypt these, the most recent relevant one will replace
    /// latest_event. (We can't tell which one is relevant until
    /// they are decrypted.)
    #[cfg(feature = "e2e-encryption")]
    pub(crate) fn latest_encrypted_events(&self) -> Vec<Raw<AnySyncTimelineEvent>> {
        self.latest_encrypted_events.read().unwrap().iter().cloned().collect()
    }

    /// Replace our latest_event with the supplied event, and delete it and all
    /// older encrypted events from latest_encrypted_events, given that the
    /// new event was at the supplied index in the latest_encrypted_events
    /// list.
    ///
    /// Panics if index is not a valid index in the latest_encrypted_events
    /// list.
    ///
    /// It is the responsibility of the caller to apply the changes into the
    /// state store after calling this function.
    #[cfg(feature = "e2e-encryption")]
    pub(crate) fn on_latest_event_decrypted(
        &self,
        latest_event: Box<LatestEvent>,
        index: usize,
        changes: &mut crate::StateChanges,
        room_info_notable_updates: &mut BTreeMap<OwnedRoomId, RoomInfoNotableUpdateReasons>,
    ) {
        self.latest_encrypted_events.write().unwrap().drain(0..=index);

        let room_info = changes
            .room_infos
            .entry(self.room_id().to_owned())
            .or_insert_with(|| self.clone_info());

        room_info.latest_event = Some(latest_event);

        room_info_notable_updates
            .entry(self.room_id().to_owned())
            .or_default()
            .insert(RoomInfoNotableUpdateReasons::LATEST_EVENT);
    }
}

#[cfg(all(test, feature = "e2e-encryption"))]
mod tests_with_e2e_encryption {
    use std::sync::Arc;

    use assert_matches::assert_matches;
    use matrix_sdk_common::deserialized_responses::TimelineEvent;
    use matrix_sdk_test::async_test;
    use ruma::{room_id, serde::Raw, user_id};
    use serde_json::json;

    use crate::{
        latest_event::LatestEvent,
        response_processors as processors,
        store::{MemoryStore, RoomLoadSettings, StoreConfig},
        BaseClient, Room, RoomInfoNotableUpdate, RoomInfoNotableUpdateReasons, RoomState,
        SessionMeta, StateChanges,
    };

    fn make_room_test_helper(room_type: RoomState) -> (Arc<MemoryStore>, Room) {
        let store = Arc::new(MemoryStore::new());
        let user_id = user_id!("@me:example.org");
        let room_id = room_id!("!test:localhost");
        let (sender, _receiver) = tokio::sync::broadcast::channel(1);

        (store.clone(), Room::new(user_id, store, room_id, room_type, sender))
    }

    #[async_test]
    async fn test_setting_the_latest_event_doesnt_cause_a_room_info_notable_update() {
        // Given a room,
        let client =
            BaseClient::new(StoreConfig::new("cross-process-store-locks-holder-name".to_owned()));

        client
            .activate(
                SessionMeta {
                    user_id: user_id!("@alice:example.org").into(),
                    device_id: ruma::device_id!("AYEAYEAYE").into(),
                },
                RoomLoadSettings::default(),
                None,
            )
            .await
            .unwrap();

        let room_id = room_id!("!test:localhost");
        let room = client.get_or_create_room(room_id, RoomState::Joined);

        // That has an encrypted event,
        add_encrypted_event(&room, "$A");
        // Sanity: it has no latest_event
        assert!(room.latest_event().is_none());

        // When I set up an observer on the latest_event,
        let mut room_info_notable_update = client.room_info_notable_update_receiver();

        // And I provide a decrypted event to replace the encrypted one,
        let event = make_latest_event("$A");

        let mut context = processors::Context::default();
        room.on_latest_event_decrypted(
            event.clone(),
            0,
            &mut context.state_changes,
            &mut context.room_info_notable_updates,
        );

        assert!(context.room_info_notable_updates.contains_key(room_id));

        // The subscriber isn't notified at this point.
        assert!(room_info_notable_update.is_empty());

        // Then updating the room info will store the event,
        processors::changes::save_and_apply(
            context,
            &client.state_store,
            &client.ignore_user_list_changes,
            None,
        )
        .await
        .unwrap();

        assert_eq!(room.latest_event().unwrap().event_id(), event.event_id());

        // And wake up the subscriber.
        assert_matches!(
            room_info_notable_update.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(reasons.contains(RoomInfoNotableUpdateReasons::LATEST_EVENT));
            }
        );
    }

    #[async_test]
    async fn test_when_we_provide_a_newly_decrypted_event_it_replaces_latest_event() {
        use std::collections::BTreeMap;

        // Given a room with an encrypted event
        let (_store, room) = make_room_test_helper(RoomState::Joined);
        add_encrypted_event(&room, "$A");
        // Sanity: it has no latest_event
        assert!(room.latest_event().is_none());

        // When I provide a decrypted event to replace the encrypted one
        let event = make_latest_event("$A");
        let mut changes = StateChanges::default();
        let mut room_info_notable_updates = BTreeMap::new();
        room.on_latest_event_decrypted(
            event.clone(),
            0,
            &mut changes,
            &mut room_info_notable_updates,
        );
        room.set_room_info(
            changes.room_infos.get(room.room_id()).cloned().unwrap(),
            room_info_notable_updates.get(room.room_id()).copied().unwrap(),
        );

        // Then is it stored
        assert_eq!(room.latest_event().unwrap().event_id(), event.event_id());
    }

    #[cfg(feature = "e2e-encryption")]
    #[async_test]
    async fn test_when_a_newly_decrypted_event_appears_we_delete_all_older_encrypted_events() {
        // Given a room with some encrypted events and a latest event

        use std::collections::BTreeMap;
        let (_store, room) = make_room_test_helper(RoomState::Joined);
        room.inner.update(|info| info.latest_event = Some(make_latest_event("$A")));
        add_encrypted_event(&room, "$0");
        add_encrypted_event(&room, "$1");
        add_encrypted_event(&room, "$2");
        add_encrypted_event(&room, "$3");

        // When I provide a latest event
        let new_event = make_latest_event("$1");
        let new_event_index = 1;
        let mut changes = StateChanges::default();
        let mut room_info_notable_updates = BTreeMap::new();
        room.on_latest_event_decrypted(
            new_event.clone(),
            new_event_index,
            &mut changes,
            &mut room_info_notable_updates,
        );
        room.set_room_info(
            changes.room_infos.get(room.room_id()).cloned().unwrap(),
            room_info_notable_updates.get(room.room_id()).copied().unwrap(),
        );

        // Then the encrypted events list is shortened to only newer events
        let enc_evs = room.latest_encrypted_events();
        assert_eq!(enc_evs.len(), 2);
        assert_eq!(enc_evs[0].get_field::<&str>("event_id").unwrap().unwrap(), "$2");
        assert_eq!(enc_evs[1].get_field::<&str>("event_id").unwrap().unwrap(), "$3");

        // And the event is stored
        assert_eq!(room.latest_event().unwrap().event_id(), new_event.event_id());
    }

    #[async_test]
    async fn test_replacing_the_newest_event_leaves_none_left() {
        use std::collections::BTreeMap;

        // Given a room with some encrypted events
        let (_store, room) = make_room_test_helper(RoomState::Joined);
        add_encrypted_event(&room, "$0");
        add_encrypted_event(&room, "$1");
        add_encrypted_event(&room, "$2");
        add_encrypted_event(&room, "$3");

        // When I provide a latest event and say it was the very latest
        let new_event = make_latest_event("$3");
        let new_event_index = 3;
        let mut changes = StateChanges::default();
        let mut room_info_notable_updates = BTreeMap::new();
        room.on_latest_event_decrypted(
            new_event,
            new_event_index,
            &mut changes,
            &mut room_info_notable_updates,
        );
        room.set_room_info(
            changes.room_infos.get(room.room_id()).cloned().unwrap(),
            room_info_notable_updates.get(room.room_id()).copied().unwrap(),
        );

        // Then the encrypted events list ie empty
        let enc_evs = room.latest_encrypted_events();
        assert_eq!(enc_evs.len(), 0);
    }

    fn add_encrypted_event(room: &Room, event_id: &str) {
        room.latest_encrypted_events
            .write()
            .unwrap()
            .push(Raw::from_json_string(json!({ "event_id": event_id }).to_string()).unwrap());
    }

    fn make_latest_event(event_id: &str) -> Box<LatestEvent> {
        Box::new(LatestEvent::new(TimelineEvent::from_plaintext(
            Raw::from_json_string(json!({ "event_id": event_id }).to_string()).unwrap(),
        )))
    }
}
