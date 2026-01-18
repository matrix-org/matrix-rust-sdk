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

use ruma::events::room::encryption::RoomEncryptionEventContent;

use super::Room;

impl Room {
    /// Get the encryption state of this room.
    pub fn encryption_state(&self) -> EncryptionState {
        self.info.read().encryption_state()
    }

    /// Get the `m.room.encryption` content that enabled end to end encryption
    /// in the room.
    pub fn encryption_settings(&self) -> Option<RoomEncryptionEventContent> {
        self.info.read().base_info.encryption.clone()
    }
}

/// Represents the state of a room encryption.
#[derive(Debug)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum EncryptionState {
    /// The room is encrypted.
    Encrypted,

    /// The room is encrypted, additionally requiring state events to be
    /// encrypted.
    #[cfg(feature = "experimental-encrypted-state-events")]
    StateEncrypted,

    /// The room is not encrypted.
    NotEncrypted,

    /// The state of the room encryption is unknown, probably because the
    /// `/sync` did not provide all data needed to decide.
    Unknown,
}

impl EncryptionState {
    /// Check whether `EncryptionState` is [`Encrypted`][Self::Encrypted].
    #[cfg(not(feature = "experimental-encrypted-state-events"))]
    pub fn is_encrypted(&self) -> bool {
        matches!(self, Self::Encrypted)
    }

    /// Check whether `EncryptionState` is [`Encrypted`][Self::Encrypted] or
    /// [`StateEncrypted`][Self::StateEncrypted].
    #[cfg(feature = "experimental-encrypted-state-events")]
    pub fn is_encrypted(&self) -> bool {
        matches!(self, Self::Encrypted | Self::StateEncrypted)
    }

    /// Check whether `EncryptionState` is
    /// [`StateEncrypted`][Self::StateEncrypted].
    #[cfg(feature = "experimental-encrypted-state-events")]
    pub fn is_state_encrypted(&self) -> bool {
        matches!(self, Self::StateEncrypted)
    }

    /// Check whether `EncryptionState` is [`Unknown`][Self::Unknown].
    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ops::{Not, Sub},
        sync::Arc,
        time::Duration,
    };

    use assert_matches::assert_matches;
    use matrix_sdk_test::{ALICE, event_factory::EventFactory};
    use ruma::{
        EventEncryptionAlgorithm, MilliSecondsSinceUnixEpoch, event_id,
        events::{AnySyncStateEvent, room::encryption::RoomEncryptionEventContent},
        room_id,
        serde::Raw,
        time::SystemTime,
        user_id,
    };

    use super::{EncryptionState, Room};
    use crate::{RoomState, store::MemoryStore, utils::RawSyncStateEventWithKeys};

    fn make_room_test_helper(room_type: RoomState) -> (Arc<MemoryStore>, Room) {
        let store = Arc::new(MemoryStore::new());
        let user_id = user_id!("@me:example.org");
        let room_id = room_id!("!test:localhost");
        let (sender, _receiver) = tokio::sync::broadcast::channel(1);

        (store.clone(), Room::new(user_id, store, room_id, room_type, sender))
    }

    fn timestamp(minutes_ago: u32) -> MilliSecondsSinceUnixEpoch {
        MilliSecondsSinceUnixEpoch::from_system_time(
            SystemTime::now().sub(Duration::from_secs((60 * minutes_ago).into())),
        )
        .expect("date out of range")
    }

    fn receive_state_events(room: &Room, events: Vec<Raw<AnySyncStateEvent>>) {
        room.info.update_if(|info| {
            let mut res = false;
            for ev in events {
                res |= info.handle_state_event(
                    &mut RawSyncStateEventWithKeys::try_from_raw_state_event(ev)
                        .expect("generated state event should be valid"),
                );
            }
            res
        });
    }

    #[test]
    fn test_encryption_is_set_when_encryption_event_is_received_encrypted() {
        let (_store, room) = make_room_test_helper(RoomState::Joined);

        assert_matches!(room.encryption_state(), EncryptionState::Unknown);

        let encryption_content =
            RoomEncryptionEventContent::new(EventEncryptionAlgorithm::MegolmV1AesSha2);
        let encryption_event = EventFactory::new()
            .sender(*ALICE)
            .event(encryption_content)
            .state_key("")
            .event_id(event_id!("$1234_1"))
            // we can simply use now here since this will be dropped when using a MinimalStateEvent
            // in the roomInfo
            .server_ts(timestamp(0))
            .into();
        receive_state_events(&room, vec![encryption_event]);

        assert_matches!(room.encryption_state(), EncryptionState::Encrypted);
    }

    #[test]
    fn test_encryption_is_set_when_encryption_event_is_received_not_encrypted() {
        let (_store, room) = make_room_test_helper(RoomState::Joined);

        assert_matches!(room.encryption_state(), EncryptionState::Unknown);
        room.info.update_if(|info| {
            info.mark_encryption_state_synced();

            false
        });

        assert_matches!(room.encryption_state(), EncryptionState::NotEncrypted);
    }

    #[test]
    fn test_encryption_state() {
        assert!(EncryptionState::Unknown.is_unknown());
        assert!(EncryptionState::Encrypted.is_unknown().not());
        assert!(EncryptionState::NotEncrypted.is_unknown().not());

        assert!(EncryptionState::Unknown.is_encrypted().not());
        assert!(EncryptionState::Encrypted.is_encrypted());
        assert!(EncryptionState::NotEncrypted.is_encrypted().not());

        #[cfg(feature = "experimental-encrypted-state-events")]
        {
            assert!(EncryptionState::StateEncrypted.is_unknown().not());
            assert!(EncryptionState::StateEncrypted.is_encrypted());

            assert!(EncryptionState::Unknown.is_state_encrypted().not());
            assert!(EncryptionState::Encrypted.is_state_encrypted().not());
            assert!(EncryptionState::StateEncrypted.is_state_encrypted());
            assert!(EncryptionState::NotEncrypted.is_state_encrypted().not());
        }
    }
}
