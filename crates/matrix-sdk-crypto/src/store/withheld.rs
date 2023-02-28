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

//! When an encrypted message is sent in a room, the group key might not be sent
//! to all devices present in the room. Sometimes this may be inadvertent (for
//! example, if the sending device is not aware of some devices that have
//! joined), but some times, this may be purposeful.
use ruma::OwnedRoomId;
use serde::{Deserialize, Serialize};
use vodozemac::Curve25519PublicKey;

use crate::types::{
    events::room_key_withheld::{MegolmV1AesSha2WithheldContent, WithheldCode},
    EventEncryptionAlgorithm,
};

///
/// We want to store when the owner of the group session sent us a withheld
/// code. It's not storing withheld code that can be sent in a
/// `m.forwarded_room_key`, it's another sort of relation as the same key can be
/// withheld for several reasons by different devices.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct DirectWithheldInfo {
    /// The room of the group key.
    pub room_id: OwnedRoomId,
    /// The key algorithm
    pub algorithm: EventEncryptionAlgorithm,
    /// The group session id.
    pub session_id: String,
    /// The session creator device identity.
    /// Claimed because withheld codes message are sent in clear
    pub claimed_sender_key: Curve25519PublicKey,
    /// The reason why the key was not distributed
    pub withheld_code: WithheldCode,
}

impl TryFrom<&MegolmV1AesSha2WithheldContent> for DirectWithheldInfo {
    type Error = &'static str;

    fn try_from(value: &MegolmV1AesSha2WithheldContent) -> Result<Self, Self::Error> {
        match value {
            MegolmV1AesSha2WithheldContent::AnyContent((code, content)) => Ok(DirectWithheldInfo {
                room_id: content.room_id.to_owned(),
                algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
                session_id: content.session_id.to_owned(),
                claimed_sender_key: content.sender_key,
                withheld_code: code.clone(),
            }),
            MegolmV1AesSha2WithheldContent::NoOlmContent(_) => {
                Err("No Olm can't be converted to direct info")
            }
        }
    }
}
