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

use ruma::RoomId;
use serde::{Deserialize, Serialize};

use crate::types::{
    events::room_key_withheld::{
        CommonWithheldCodeContent, MegolmV1AesSha2WithheldContent, RoomKeyWithheldContent,
        RoomKeyWithheldEvent, WithheldCode,
    },
    EventEncryptionAlgorithm,
};

/// We want to store when the owner of the group session sent us a withheld
/// code. It's not storing withheld code that can be sent in a
/// `m.forwarded_room_key`, it's another sort of relation as the same key can be
/// withheld for several reasons by different devices.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub enum DirectWithheldInfo {
    MegolmV1(MegolmV1WithheldInfo),
    #[cfg(feature = "experimental-algorithms")]
    MegolmV2(MegolmV2WithheldInfo),
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub enum MegolmV1WithheldInfo {
    BlackListed(Box<CommonWithheldCodeContent>),
    Unverified(Box<CommonWithheldCodeContent>),
}

#[cfg(feature = "experimental-algorithms")]
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub enum MegolmV2WithheldInfo {
    BlackListed(Box<CommonWithheldCodeContent>),
    Unverified(Box<CommonWithheldCodeContent>),
}

impl DirectWithheldInfo {
    pub fn new(
        algorithm: EventEncryptionAlgorithm,
        code: WithheldCode,
        content: CommonWithheldCodeContent,
    ) -> Self {
        let content = content.into();

        match algorithm {
            EventEncryptionAlgorithm::MegolmV1AesSha2 => match code {
                WithheldCode::Blacklisted => {
                    Self::MegolmV1(MegolmV1WithheldInfo::BlackListed(content))
                }
                WithheldCode::Unverified => {
                    Self::MegolmV1(MegolmV1WithheldInfo::Unverified(content))
                }
                _ => panic!("Unsupported direct withheld code"),
            },
            #[cfg(feature = "experimental-algorithms")]
            EventEncryptionAlgorithm::MegolmV1AesSha2 => match code {
                WithheldCode::Blacklisted => {
                    Self::MegolmV2(MegolmV2WithheldInfo::BlackListed(content))
                }
                WithheldCode::Unverified => {
                    Self::MegolmV2(MegolmV2WithheldInfo::Unverified(content))
                }
                _ => panic!("Unsupported direct withheld code"),
            },
            _ => todo!(),
        }
    }

    pub fn from_event(event: &RoomKeyWithheldEvent) -> Option<Self> {
        match &event.content {
            RoomKeyWithheldContent::MegolmV1AesSha2(c) => match c {
                MegolmV1AesSha2WithheldContent::BlackListed(c) => {
                    Some(Self::MegolmV1(MegolmV1WithheldInfo::BlackListed(c.to_owned())))
                }
                MegolmV1AesSha2WithheldContent::Unverified(c) => {
                    Some(Self::MegolmV1(MegolmV1WithheldInfo::Unverified(c.to_owned())))
                }
                MegolmV1AesSha2WithheldContent::NoOlm(_) => todo!(),
                _ => None,
            },
            #[cfg(feature = "experimental-algorithms")]
            RoomKeyWithheldContent::MegolmV2AesSha2(c) => match c {
                MegolmV2AesSha2WithheldContent::BlackListed(_) => todo!(),
                MegolmV2AesSha2WithheldContent::Unverified(_) => todo!(),
                MegolmV2AesSha2WithheldContent::NoOlm(_) => todo!(),
            },
            RoomKeyWithheldContent::Unknown(_) => None,
        }
    }

    pub fn algorithm(&self) -> EventEncryptionAlgorithm {
        match self {
            DirectWithheldInfo::MegolmV1(_) => EventEncryptionAlgorithm::MegolmV1AesSha2,
            #[cfg(feature = "experimental-algorithms")]
            DirectWithheldInfo::MegolmV2(_) => EventEncryptionAlgorithm::MegolmV2AesSha2,
        }
    }

    pub fn withheld_code(&self) -> WithheldCode {
        match self {
            DirectWithheldInfo::MegolmV1(c) => match c {
                MegolmV1WithheldInfo::BlackListed(_) => WithheldCode::Blacklisted,
                MegolmV1WithheldInfo::Unverified(_) => WithheldCode::Unverified,
            },
            #[cfg(feature = "experimental-algorithms")]
            DirectWithheldInfo::MegolmV2(c) => match c {
                MegolmV2WithheldInfo::BlackListed(_) => WithheldCode::Blacklisted,
                MegolmV2WithheldInfo::Unverified(_) => WithheldCode::Unverified,
            },
        }
    }

    pub fn room_id(&self) -> &RoomId {
        match self {
            DirectWithheldInfo::MegolmV1(c) => match c {
                MegolmV1WithheldInfo::BlackListed(c) | MegolmV1WithheldInfo::Unverified(c) => {
                    &c.room_id
                }
            },
            #[cfg(feature = "experimental-algorithms")]
            DirectWithheldInfo::MegolmV2(c) => match c {
                MegolmV2WithheldInfo::BlackListed(c) | MegolmV2WithheldInfo::Unverified(c) => {
                    &c.room_id
                }
            },
        }
    }

    pub fn session_id(&self) -> &str {
        match self {
            DirectWithheldInfo::MegolmV1(c) => match c {
                MegolmV1WithheldInfo::BlackListed(c) | MegolmV1WithheldInfo::Unverified(c) => {
                    &c.session_id
                }
            },
            #[cfg(feature = "experimental-algorithms")]
            DirectWithheldInfo::MegolmV2(c) => match c {
                MegolmV2WithheldInfo::BlackListed(c) | MegolmV2WithheldInfo::Unverified(c) => {
                    &c.session_id
                }
            },
        }
    }
}
