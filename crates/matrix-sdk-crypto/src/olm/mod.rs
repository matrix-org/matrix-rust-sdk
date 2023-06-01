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

//! The crypto specific Olm objects.
//!
//! Note: You'll only be interested in these if you are implementing a custom
//! `CryptoStore`.

mod account;
mod group_sessions;
mod session;
mod signing;
mod utility;

pub(crate) use account::{Account, OlmDecryptionInfo, SessionType};
pub use account::{OlmMessageHash, PickledAccount, ReadOnlyAccount};
pub(crate) use group_sessions::ShareState;
pub use group_sessions::{
    BackedUpRoomKey, EncryptionSettings, ExportedRoomKey, InboundGroupSession,
    OutboundGroupSession, PickledInboundGroupSession, PickledOutboundGroupSession,
    SessionCreationError, SessionExportError, SessionKey, ShareInfo,
};
pub use session::{PickledSession, Session};
pub use signing::{CrossSigningStatus, PickledCrossSigningIdentity, PrivateCrossSigningIdentity};
pub(crate) use utility::{SignedJsonObject, VerifyJson};
pub use vodozemac::{olm::IdentityKeys, Curve25519PublicKey};

#[cfg(test)]
pub(crate) mod tests {
    use assert_matches::assert_matches;
    use matrix_sdk_test::async_test;
    use ruma::{
        device_id, event_id,
        events::{
            relation::Replacement,
            room::message::{Relation, RoomMessageEventContent},
            AnyMessageLikeEvent, AnyTimelineEvent, MessageLikeEvent,
        },
        room_id, user_id, DeviceId, UserId,
    };
    use serde_json::{json, Value};
    use vodozemac::{
        olm::{OlmMessage, SessionConfig},
        Curve25519PublicKey, Ed25519PublicKey,
    };

    use crate::{
        olm::{ExportedRoomKey, InboundGroupSession, ReadOnlyAccount, Session},
        types::events::{
            forwarded_room_key::ForwardedRoomKeyContent, room::encrypted::EncryptedEvent,
        },
        utilities::json_convert,
    };

    fn alice_id() -> &'static UserId {
        user_id!("@alice:example.org")
    }

    fn alice_device_id() -> &'static DeviceId {
        device_id!("ALICEDEVICE")
    }

    fn bob_id() -> &'static UserId {
        user_id!("@bob:example.org")
    }

    fn bob_device_id() -> &'static DeviceId {
        device_id!("BOBDEVICE")
    }

    pub(crate) async fn get_account_and_session() -> (ReadOnlyAccount, Session) {
        let alice = ReadOnlyAccount::new(alice_id(), alice_device_id());
        let bob = ReadOnlyAccount::new(bob_id(), bob_device_id());

        bob.generate_one_time_keys_helper(1).await;
        let one_time_key = *bob.one_time_keys().await.values().next().unwrap();
        let sender_key = bob.identity_keys().curve25519;
        let session = alice
            .create_outbound_session_helper(
                SessionConfig::default(),
                sender_key,
                one_time_key,
                false,
            )
            .await;

        (alice, session)
    }

    #[test]
    fn account_creation() {
        let account = ReadOnlyAccount::new(alice_id(), alice_device_id());

        assert!(!account.shared());

        account.mark_as_shared();
        assert!(account.shared());
    }

    #[async_test]
    async fn one_time_keys_creation() {
        let account = ReadOnlyAccount::new(alice_id(), alice_device_id());
        let one_time_keys = account.one_time_keys().await;

        assert!(one_time_keys.is_empty());
        assert_ne!(account.max_one_time_keys().await, 0);

        account.generate_one_time_keys_helper(10).await;
        let one_time_keys = account.one_time_keys().await;

        assert_ne!(one_time_keys.values().len(), 0);
        assert_ne!(one_time_keys.keys().len(), 0);
        assert_ne!(one_time_keys.iter().len(), 0);

        account.mark_keys_as_published().await;
        let one_time_keys = account.one_time_keys().await;
        assert!(one_time_keys.is_empty());
    }

    #[async_test]
    async fn session_creation() {
        let alice = ReadOnlyAccount::new(alice_id(), alice_device_id());
        let bob = ReadOnlyAccount::new(bob_id(), bob_device_id());
        let alice_keys = alice.identity_keys();
        alice.generate_one_time_keys_helper(1).await;
        let one_time_keys = alice.one_time_keys().await;
        alice.mark_keys_as_published().await;

        let one_time_key = *one_time_keys.values().next().unwrap();

        let mut bob_session = bob
            .create_outbound_session_helper(
                SessionConfig::default(),
                alice_keys.curve25519,
                one_time_key,
                false,
            )
            .await;

        let plaintext = "Hello world";

        let message = bob_session.encrypt_helper(plaintext).await;

        let prekey_message = match message.clone() {
            OlmMessage::PreKey(m) => m,
            OlmMessage::Normal(_) => panic!("Incorrect message type"),
        };

        let bob_keys = bob.identity_keys();
        let result =
            alice.create_inbound_session(bob_keys.curve25519, &prekey_message).await.unwrap();

        assert_eq!(bob_session.session_id(), result.session.session_id());

        assert_eq!(plaintext, result.plaintext);
    }

    #[async_test]
    async fn group_session_creation() {
        let alice = ReadOnlyAccount::new(alice_id(), alice_device_id());
        let room_id = room_id!("!test:localhost");

        let (outbound, _) = alice.create_group_session_pair_with_defaults(room_id).await;

        assert_eq!(0, outbound.message_index().await);
        assert!(!outbound.shared());
        outbound.mark_as_shared();
        assert!(outbound.shared());

        let inbound = InboundGroupSession::new(
            Curve25519PublicKey::from_base64("Nn0L2hkcCMFKqynTjyGsJbth7QrVmX3lbrksMkrGOAw")
                .unwrap(),
            Ed25519PublicKey::from_base64("ee3Ek+J2LkkPmjGPGLhMxiKnhiX//xcqaVL4RP6EypE").unwrap(),
            room_id,
            &outbound.session_key().await,
            outbound.settings().algorithm.to_owned(),
            None,
        )
        .expect("We can always create an inbound group session from an outbound one");

        assert_eq!(0, inbound.first_known_index());

        assert_eq!(outbound.session_id(), inbound.session_id());

        let plaintext = "This is a secret to everybody".to_owned();
        let ciphertext = outbound.encrypt_helper(plaintext.clone()).await;

        assert_eq!(
            plaintext.as_bytes(),
            inbound.decrypt_helper(&ciphertext).await.unwrap().plaintext
        );
    }

    #[async_test]
    async fn edit_decryption() {
        let alice = ReadOnlyAccount::new(alice_id(), alice_device_id());
        let room_id = room_id!("!test:localhost");
        let event_id = event_id!("$1234adfad:asdf");

        let (outbound, _) = alice.create_group_session_pair_with_defaults(room_id).await;

        assert_eq!(0, outbound.message_index().await);
        assert!(!outbound.shared());
        outbound.mark_as_shared();
        assert!(outbound.shared());

        let mut content = RoomMessageEventContent::text_plain("Hello");
        content.relates_to = Some(Relation::Replacement(Replacement::new(
            event_id.to_owned(),
            RoomMessageEventContent::text_plain("Hello edit").into(),
        )));

        let inbound = InboundGroupSession::new(
            Curve25519PublicKey::from_base64("Nn0L2hkcCMFKqynTjyGsJbth7QrVmX3lbrksMkrGOAw")
                .unwrap(),
            Ed25519PublicKey::from_base64("ee3Ek+J2LkkPmjGPGLhMxiKnhiX//xcqaVL4RP6EypE").unwrap(),
            room_id,
            &outbound.session_key().await,
            outbound.settings().algorithm.to_owned(),
            None,
        )
        .unwrap();

        assert_eq!(0, inbound.first_known_index());

        assert_eq!(outbound.session_id(), inbound.session_id());

        let encrypted_content =
            outbound.encrypt(serde_json::to_value(content).unwrap(), "m.room.message").await;

        let event = json!({
            "sender": alice.user_id(),
            "event_id": event_id,
            "origin_server_ts": 0u64,
            "room_id": room_id,
            "type": "m.room.encrypted",
            "content": encrypted_content,
        });

        let event = json_convert(&event).unwrap();
        let decrypted = inbound.decrypt(&event).await.unwrap().0;

        if let AnyTimelineEvent::MessageLike(AnyMessageLikeEvent::RoomMessage(
            MessageLikeEvent::Original(e),
        )) = decrypted.deserialize().unwrap()
        {
            assert_matches!(e.content.relates_to, Some(Relation::Replacement(_)));
        } else {
            panic!("Invalid event type")
        }
    }

    #[async_test]
    async fn relates_to_decryption() {
        let alice = ReadOnlyAccount::new(alice_id(), alice_device_id());
        let room_id = room_id!("!test:localhost");
        let event_id = event_id!("$1234adfad:asdf");

        let relation_json = json!({
            "rel_type": "m.reference",
            "event_id": "$WUreEJERkFzO8i2dk6CmTex01cP1dZ4GWKhKCwkWHrQ"
        });

        let (outbound, inbound) = alice.create_group_session_pair_with_defaults(room_id).await;

        // We first test that we're copying the relation from the content that
        // will be encrypted to the content that will stay plaintext.
        let content = json!({
            "m.relates_to": relation_json,
        });
        let encrypted = outbound.encrypt(content, "m.dummy").await;

        let event = json!({
            "sender": alice.user_id(),
            "event_id": event_id,
            "origin_server_ts": 0u64,
            "room_id": room_id,
            "type": "m.room.encrypted",
            "content": encrypted,
        });
        let event: EncryptedEvent = json_convert(&event).unwrap();

        assert_eq!(
            event.content.relates_to.as_ref(),
            Some(&relation_json),
            "The encrypted event should contain an unencrypted relation"
        );

        let (decrypted, _) = inbound.decrypt(&event).await.unwrap();

        let decrypted: Value = json_convert(&decrypted).unwrap();
        let relation = decrypted.get("content").and_then(|c| c.get("m.relates_to"));
        assert_eq!(relation, Some(&relation_json), "The decrypted event should contain a relation");

        let content = json!({});
        let encrypted = outbound.encrypt(content, "m.dummy").await;
        let mut encrypted: Value = json_convert(&encrypted).unwrap();
        encrypted.as_object_mut().unwrap().insert("m.relates_to".to_owned(), relation_json.clone());

        // Let's now test if we copy the correct relation if there is no
        // relation in the encrypted content.
        let event = json!({
            "sender": alice.user_id(),
            "event_id": event_id,
            "origin_server_ts": 0u64,
            "room_id": room_id,
            "type": "m.room.encrypted",
            "content": encrypted,
        });
        let event: EncryptedEvent = json_convert(&event).unwrap();

        assert_eq!(
            event.content.relates_to,
            Some(relation_json),
            "The encrypted event should contain an unencrypted relation"
        );

        let (decrypted, _) = inbound.decrypt(&event).await.unwrap();

        let decrypted: Value = json_convert(&decrypted).unwrap();
        let relation = decrypted.get("content").and_then(|c| c.get("m.relates_to"));
        assert!(relation.is_some(), "The decrypted event should contain a relation");
    }

    #[async_test]
    async fn group_session_export() {
        let alice = ReadOnlyAccount::new(alice_id(), alice_device_id());
        let room_id = room_id!("!test:localhost");

        let (_, inbound) = alice.create_group_session_pair_with_defaults(room_id).await;

        let export = inbound.export().await;
        let export: ForwardedRoomKeyContent = export.try_into().unwrap();
        let export = ExportedRoomKey::try_from(export).unwrap();

        let imported = InboundGroupSession::from_export(&export)
            .expect("We can always import an inbound group session from a fresh export");

        assert_eq!(inbound.session_id(), imported.session_id());
    }
}
