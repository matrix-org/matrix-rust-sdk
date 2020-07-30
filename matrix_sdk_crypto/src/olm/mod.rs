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

mod account;
mod group_sessions;
mod session;

pub use account::{Account, IdentityKeys};
pub use group_sessions::{GroupSessionKey, InboundGroupSession, OutboundGroupSession};
pub use session::{OlmMessage, Session};

#[cfg(test)]
pub(crate) mod test {
    use crate::olm::{Account, InboundGroupSession, Session};
    use matrix_sdk_common::{
        api::r0::keys::SignedKey,
        identifiers::{DeviceId, RoomId, UserId},
    };
    use olm_rs::session::OlmMessage;
    use std::{collections::BTreeMap, convert::TryFrom};

    fn alice_id() -> UserId {
        UserId::try_from("@alice:example.org").unwrap()
    }

    fn alice_device_id() -> Box<DeviceId> {
        "ALICEDEVICE".into()
    }

    fn bob_id() -> UserId {
        UserId::try_from("@bob:example.org").unwrap()
    }

    fn bob_device_id() -> Box<DeviceId> {
        "BOBDEVICE".into()
    }

    pub(crate) async fn get_account_and_session() -> (Account, Session) {
        let alice = Account::new(&alice_id(), &alice_device_id());
        let bob = Account::new(&bob_id(), &bob_device_id());

        bob.generate_one_time_keys_helper(1).await;
        let one_time_key = bob
            .one_time_keys()
            .await
            .curve25519()
            .iter()
            .next()
            .unwrap()
            .1
            .to_owned();
        let one_time_key = SignedKey {
            key: one_time_key,
            signatures: BTreeMap::new(),
        };
        let sender_key = bob.identity_keys().curve25519().to_owned();
        let session = alice
            .create_outbound_session_helper(&sender_key, &one_time_key)
            .await
            .unwrap();

        (alice, session)
    }

    #[test]
    fn account_creation() {
        let account = Account::new(&alice_id(), &alice_device_id());
        let identyty_keys = account.identity_keys();

        assert!(!account.shared());
        assert!(!identyty_keys.ed25519().is_empty());
        assert_ne!(identyty_keys.values().len(), 0);
        assert_ne!(identyty_keys.keys().len(), 0);
        assert_ne!(identyty_keys.iter().len(), 0);
        assert!(identyty_keys.contains_key("ed25519"));
        assert_eq!(
            identyty_keys.ed25519(),
            identyty_keys.get("ed25519").unwrap()
        );
        assert!(!identyty_keys.curve25519().is_empty());

        account.mark_as_shared();
        assert!(account.shared());
    }

    #[tokio::test]
    async fn one_time_keys_creation() {
        let account = Account::new(&alice_id(), &alice_device_id());
        let one_time_keys = account.one_time_keys().await;

        assert!(one_time_keys.curve25519().is_empty());
        assert_ne!(account.max_one_time_keys().await, 0);

        account.generate_one_time_keys_helper(10).await;
        let one_time_keys = account.one_time_keys().await;

        assert!(!one_time_keys.curve25519().is_empty());
        assert_ne!(one_time_keys.values().len(), 0);
        assert_ne!(one_time_keys.keys().len(), 0);
        assert_ne!(one_time_keys.iter().len(), 0);
        assert!(one_time_keys.contains_key("curve25519"));
        assert_eq!(one_time_keys.curve25519().keys().len(), 10);
        assert_eq!(
            one_time_keys.curve25519(),
            one_time_keys.get("curve25519").unwrap()
        );

        account.mark_keys_as_published().await;
        let one_time_keys = account.one_time_keys().await;
        assert!(one_time_keys.curve25519().is_empty());
    }

    #[tokio::test]
    async fn session_creation() {
        let alice = Account::new(&alice_id(), &alice_device_id());
        let bob = Account::new(&bob_id(), &bob_device_id());
        let alice_keys = alice.identity_keys();
        alice.generate_one_time_keys_helper(1).await;
        let one_time_keys = alice.one_time_keys().await;
        alice.mark_keys_as_published().await;

        let one_time_key = one_time_keys
            .curve25519()
            .iter()
            .next()
            .unwrap()
            .1
            .to_owned();

        let one_time_key = SignedKey {
            key: one_time_key,
            signatures: BTreeMap::new(),
        };

        let mut bob_session = bob
            .create_outbound_session_helper(alice_keys.curve25519(), &one_time_key)
            .await
            .unwrap();

        let plaintext = "Hello world";

        let message = bob_session.encrypt_helper(plaintext).await;

        let prekey_message = match message.clone() {
            OlmMessage::PreKey(m) => m,
            OlmMessage::Message(_) => panic!("Incorrect message type"),
        };

        let bob_keys = bob.identity_keys();
        let mut alice_session = alice
            .create_inbound_session(bob_keys.curve25519(), prekey_message.clone())
            .await
            .unwrap();

        assert!(alice_session
            .matches(bob_keys.curve25519(), prekey_message)
            .await
            .unwrap());

        assert_eq!(bob_session.session_id(), alice_session.session_id());

        let decyrpted = alice_session.decrypt(message).await.unwrap();
        assert_eq!(plaintext, decyrpted);
    }

    #[tokio::test]
    async fn group_session_creation() {
        let alice = Account::new(&alice_id(), &alice_device_id());
        let room_id = RoomId::try_from("!test:localhost").unwrap();

        let (outbound, _) = alice.create_group_session_pair(&room_id).await;

        assert_eq!(0, outbound.message_index().await);
        assert!(!outbound.shared());
        outbound.mark_as_shared();
        assert!(outbound.shared());

        let inbound = InboundGroupSession::new(
            "test_key",
            "test_key",
            &room_id,
            outbound.session_key().await,
        )
        .unwrap();

        assert_eq!(0, inbound.first_known_index().await);

        assert_eq!(outbound.session_id(), inbound.session_id());

        let plaintext = "This is a secret to everybody".to_owned();
        let ciphertext = outbound.encrypt_helper(plaintext.clone()).await;

        assert_eq!(
            plaintext,
            inbound.decrypt_helper(ciphertext).await.unwrap().0
        );
    }
}
