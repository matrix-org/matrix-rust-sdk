// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use std::{collections::BTreeMap, convert::TryInto};

use ruma::{
    events::forwarded_room_key::{
        ToDeviceForwardedRoomKeyEventContent, ToDeviceForwardedRoomKeyEventContentInit,
    },
    DeviceKeyAlgorithm, EventEncryptionAlgorithm, RoomId,
};
use serde::{Deserialize, Serialize};

mod inbound;
mod outbound;

pub use inbound::{InboundGroupSession, PickledInboundGroupSession};
pub(crate) use outbound::ShareState;
pub use outbound::{
    EncryptionSettings, GroupSession, OutboundGroupSession, PickledOutboundGroupSession, ShareInfo,
};
pub use vodozemac::megolm::{ExportedSessionKey, SessionKey};

/// An exported version of an `InboundGroupSession`
///
/// This can be used to share the `InboundGroupSession` in an exported file.
#[derive(Deserialize, Serialize)]
#[allow(missing_debug_implementations)]
pub struct ExportedRoomKey {
    /// The encryption algorithm that the session uses.
    pub algorithm: EventEncryptionAlgorithm,

    /// The room where the session is used.
    pub room_id: Box<RoomId>,

    /// The Curve25519 key of the device which initiated the session originally.
    pub sender_key: String,

    /// The ID of the session that the key is for.
    pub session_id: String,

    /// The key for the session.
    pub session_key: ExportedSessionKey,

    /// The Ed25519 key of the device which initiated the session originally.
    #[serde(default)]
    pub sender_claimed_keys: BTreeMap<DeviceKeyAlgorithm, String>,

    /// Chain of Curve25519 keys through which this session was forwarded, via
    /// m.forwarded_room_key events.
    #[serde(default)]
    pub forwarding_curve25519_key_chain: Vec<String>,
}

/// A backed up version of an `InboundGroupSession`
///
/// This can be used to backup the `InboundGroupSession` to the server.
#[derive(Deserialize, Serialize)]
#[allow(missing_debug_implementations)]
pub struct BackedUpRoomKey {
    /// The encryption algorithm that the session uses.
    pub algorithm: EventEncryptionAlgorithm,

    /// The Curve25519 key of the device which initiated the session originally.
    pub sender_key: String,

    /// The key for the session.
    pub session_key: ExportedSessionKey,

    /// The Ed25519 key of the device which initiated the session originally.
    pub sender_claimed_keys: BTreeMap<DeviceKeyAlgorithm, String>,

    /// Chain of Curve25519 keys through which this session was forwarded, via
    /// m.forwarded_room_key events.
    pub forwarding_curve25519_key_chain: Vec<String>,
}

impl TryInto<ToDeviceForwardedRoomKeyEventContent> for ExportedRoomKey {
    type Error = ();

    /// Convert an exported room key into a content for a forwarded room key
    /// event.
    ///
    /// This will fail if the exported room key has multiple sender claimed keys
    /// or if the algorithm of the claimed sender key isn't
    /// `DeviceKeyAlgorithm::Ed25519`.
    fn try_into(self) -> Result<ToDeviceForwardedRoomKeyEventContent, Self::Error> {
        if self.sender_claimed_keys.len() != 1 {
            Err(())
        } else {
            let (algorithm, claimed_key) = self.sender_claimed_keys.iter().next().ok_or(())?;

            if algorithm != &DeviceKeyAlgorithm::Ed25519 {
                return Err(());
            }

            Ok(ToDeviceForwardedRoomKeyEventContentInit {
                algorithm: self.algorithm,
                room_id: self.room_id,
                sender_key: self.sender_key,
                session_id: self.session_id,
                session_key: self.session_key.0.clone(),
                sender_claimed_ed25519_key: claimed_key.to_owned(),
                forwarding_curve25519_key_chain: self.forwarding_curve25519_key_chain,
            }
            .into())
        }
    }
}

impl From<ExportedRoomKey> for BackedUpRoomKey {
    fn from(k: ExportedRoomKey) -> Self {
        Self {
            algorithm: k.algorithm,
            sender_key: k.sender_key,
            session_key: k.session_key,
            sender_claimed_keys: k.sender_claimed_keys,
            forwarding_curve25519_key_chain: k.forwarding_curve25519_key_chain,
        }
    }
}

impl From<ToDeviceForwardedRoomKeyEventContent> for ExportedRoomKey {
    /// Convert the content of a forwarded room key into a exported room key.
    fn from(forwarded_key: ToDeviceForwardedRoomKeyEventContent) -> Self {
        let mut sender_claimed_keys: BTreeMap<DeviceKeyAlgorithm, String> = BTreeMap::new();
        sender_claimed_keys
            .insert(DeviceKeyAlgorithm::Ed25519, forwarded_key.sender_claimed_ed25519_key);

        Self {
            algorithm: forwarded_key.algorithm,
            room_id: forwarded_key.room_id,
            session_id: forwarded_key.session_id,
            forwarding_curve25519_key_chain: forwarded_key.forwarding_curve25519_key_chain,
            sender_claimed_keys,
            sender_key: forwarded_key.sender_key,
            session_key: ExportedSessionKey(forwarded_key.session_key),
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_arch = "wasm32")))]
mod test {
    use std::{sync::Arc, time::Duration};

    use matrix_sdk_common::instant::Instant;
    use matrix_sdk_test::async_test;
    use ruma::{device_id, events::room::message::RoomMessageEventContent, room_id, user_id};

    use super::EncryptionSettings;
    use crate::{MegolmError, ReadOnlyAccount};

    #[async_test]
    async fn expiration() -> Result<(), MegolmError> {
        let settings = EncryptionSettings { rotation_period_msgs: 1, ..Default::default() };

        let account = ReadOnlyAccount::new(user_id!("@alice:example.org"), device_id!("DEVICEID"));
        let (session, _) = account
            .create_group_session_pair(room_id!("!test_room:example.org"), settings)
            .await
            .unwrap();

        assert!(!session.expired());
        let _ = session
            .encrypt(
                serde_json::to_value(RoomMessageEventContent::text_plain("Test message"))?,
                "m.room.message",
            )
            .await;
        assert!(session.expired());

        let settings = EncryptionSettings {
            rotation_period: Duration::from_millis(100),
            ..Default::default()
        };

        let (mut session, _) = account
            .create_group_session_pair(room_id!("!test_room:example.org"), settings)
            .await
            .unwrap();

        assert!(!session.expired());
        // FIXME: this might break on macosx and windows
        session.creation_time = Arc::new(Instant::now() - Duration::from_secs(60 * 60));
        assert!(session.expired());

        Ok(())
    }
}
