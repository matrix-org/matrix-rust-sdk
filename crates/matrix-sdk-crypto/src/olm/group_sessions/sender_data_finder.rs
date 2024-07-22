// Copyright 2024 The Matrix.org Foundation C.I.C.
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

use ruma::UserId;
use tracing::error;
use vodozemac::Curve25519PublicKey;

use super::{SenderData, SenderDataRetryDetails};
use crate::{
    error::{OlmResult, SessionCreationError},
    store::Store,
    types::{events::olm_v1::DecryptedRoomKeyEvent, DeviceKeys},
    Device, DeviceData, OlmError,
};

/// Temporary struct that is used to look up [`SenderData`] based on the
/// information supplied in
/// [`crate::types::events::olm_v1::DecryptedRoomKeyEvent`].
///
/// # Algorithm
///
/// When we receive a to-device message establishing a megolm session (i.e. when
/// [`crate::machine::OlmMachine::add_room_key`] is called):
///
/// ┌───────────────────────────────────────────────────────────────────┐
/// │ A (start - we have a to-device message containing a room key)     │
/// └───────────────────────────────────────────────────────────────────┘
///                                     │
///   __________________________________▼______________________________
///  ╱                                                                 ╲
/// ╱ Does the to-device message contain the device_keys property from  ╲yes
/// ╲ MSC4147?                                                          ╱ │
///  ╲_________________________________________________________________╱  │
///                                     │ no                              │
///                                     ▼                                 │
/// ┌───────────────────────────────────────────────────────────────────┐ │
/// │ B (there are no device keys in the to-device message)             │ │
/// │                                                                   │ │
/// │ We need to find the device details.                               │ │
/// └───────────────────────────────────────────────────────────────────┘ │
///                                     │                                 │
///   __________________________________▼______________________________   │
///  ╱                                                                 ╲  │
/// ╱ Does the store contain a device whose curve key matches the       ╲ ▼
/// ╲ sender of the to-device message?                                  ╱yes
///  ╲_________________________________________________________________╱  │
///                                     │ no                              │
///                                     ▼                                 │
/// ╭───────────────────────────────────────────────────────────────────╮ │
/// │ C (we don't know the sending device)                              │ │
/// │                                                                   │ │
/// │ Give up: we have no sender info for this room key.                │ │
/// ╰───────────────────────────────────────────────────────────────────╯ │
///                                     ┌─────────────────────────────────┘
///                                     ▼
/// ┌───────────────────────────────────────────────────────────────────┐
/// │ D (we have the device)                                            │
/// └───────────────────────────────────────────────────────────────────┘
///                                     │
///   __________________________________▼______________________________
///  ╱                                                                 ╲
/// ╱ Is the device cross-signed by the sender?                         ╲yes
/// ╲___________________________________________________________________╱ │
///                                     │ no                              │
///                                     ▼                                 │
/// ┌───────────────────────────────────────────────────────────────────┐ │
/// │ F (we have device keys, but they are not signed by the sender)    │ │
/// │                                                                   │ │
/// │ Store the device with the session, in case we can confirm it      │ │
/// │ later.                                                            │ │
/// ╰───────────────────────────────────────────────────────────────────╯ │
///                                     ┌─────────────────────────────────┘
///                                     ▼
/// ┌───────────────────────────────────────────────────────────────────┐
/// │ G (device is cross-signed by the sender)                          │
/// └───────────────────────────────────────────────────────────────────┘
///                                     │
///   __________________________________▼______________________________
///  ╱                                                                 ╲
/// ╱ Does the cross-signing key match that used                        ╲yes
/// ╲ to sign the device?                                               ╱ │
///  ╲_________________________________________________________________╱  │
///                                     │ no                              │
///                                     ▼                                 │
/// ╭───────────────────────────────────────────────────────────────────╮ │
/// │ Store the device with the session, in case we get the             │ │
/// │ right cross-signing key later.                                    │ │
/// ╰───────────────────────────────────────────────────────────────────╯ │
///                                     ┌─────────────────────────────────┘
///                                     ▼
/// ┌───────────────────────────────────────────────────────────────────┐
/// │ H (cross-signing key matches that used to sign the device!)       │
/// │                                                                   │
/// │ Look up the user_id and master_key for the user sending the       │
/// │ to-device message.                                                │
/// │                                                                   │
/// │ Decide the master_key trust level based on whether we have        │
/// │ verified this user.                                               │
/// │                                                                   │
/// │ Store this information with the session.                          │
/// ╰───────────────────────────────────────────────────────────────────╯
///
/// Note: the sender data may become out-of-date if we later verify the user. We
/// have no plans to update it if so.
pub(crate) struct SenderDataFinder<'a> {
    store: &'a Store,
}

impl<'a> SenderDataFinder<'a> {
    /// Find the device associated with the to-device message used to
    /// create the InboundGroupSession we are about to create, and decide
    /// whether we trust the sender.
    pub(crate) async fn find_using_event(
        store: &'a Store,
        sender_curve_key: Curve25519PublicKey,
        room_key_event: &'a DecryptedRoomKeyEvent,
    ) -> OlmResult<SenderData> {
        let finder = Self { store };
        finder.have_event(sender_curve_key, room_key_event).await
    }

    /// Use the supplied device keys to decide whether we trust the sender.
    pub(crate) async fn find_using_device_keys(
        store: &'a Store,
        device_keys: DeviceKeys,
    ) -> OlmResult<SenderData> {
        let finder = Self { store };
        finder.have_device_keys(&device_keys).await
    }

    /// Step A (start - we have a to-device message containing a room key)
    async fn have_event(
        &self,
        sender_curve_key: Curve25519PublicKey,
        room_key_event: &'a DecryptedRoomKeyEvent,
    ) -> OlmResult<SenderData> {
        // Does the to-device message contain the device_keys property from MSC4147?
        if let Some(sender_device_keys) = &room_key_event.device_keys {
            // Yes: use the device keys to continue
            self.have_device_keys(sender_device_keys).await
        } else {
            // No: look for the device in the store
            self.search_for_device(sender_curve_key, &room_key_event.sender).await
        }
    }

    /// Step B (there are no device keys in the to-device message)
    async fn search_for_device(
        &self,
        sender_curve_key: Curve25519PublicKey,
        sender_user_id: &UserId,
    ) -> OlmResult<SenderData> {
        // Does the locally-cached (in the store) devices list contain a device with the
        // curve key of the sender of the to-device message?
        if let Some(sender_device) =
            self.store.get_device_from_curve_key(sender_user_id, sender_curve_key).await?
        {
            // Yes: use the device to continue
            self.have_device(sender_device)
        } else {
            // Step C (we don't know the sending device)
            //
            // We have no device data for this session so we can't continue in the "fast
            // lane" (blocking sync).
            let sender_data = SenderData::UnknownDevice {
                retry_details: SenderDataRetryDetails::retry_soon(),
                // This is not a legacy session since we did attempt to look
                // up its sender data at the time of reception.
                // legacy_session: false,
                // TODO: we set legacy to true for now, since our implementation is incomplete, so
                // we may not have had a proper chance to look up the sender data.
                legacy_session: true,
            };
            Ok(sender_data)
        }
    }

    async fn have_device_keys(&self, sender_device_keys: &DeviceKeys) -> OlmResult<SenderData> {
        // Validate the signature of the DeviceKeys supplied.
        match DeviceData::try_from(sender_device_keys) {
            Ok(sender_device_data) => {
                let sender_device = self.store.wrap_device_data(sender_device_data).await?;

                self.have_device(sender_device)
            }
            Err(e) => {
                // The device keys supplied did not validate.
                Err(OlmError::SessionCreation(SessionCreationError::InvalidDeviceKeys(e)))
            }
        }
    }

    /// Step D (we have a device)
    fn have_device(&self, sender_device: Device) -> OlmResult<SenderData> {
        // Is the device cross-signed?
        // Does the cross-signing key match that used to sign the device?
        // And is the signature in the device valid?

        if sender_device.is_cross_signed_by_owner() {
            // Yes: check the device is signed by the sender
            self.device_is_cross_signed_by_sender(sender_device)
        } else {
            // No: F (we have device keys, but they are not signed by the sender)
            Ok(SenderData::DeviceInfo {
                device_keys: sender_device.as_device_keys().clone(),
                retry_details: SenderDataRetryDetails::retry_soon(),
                legacy_session: true, // TODO: change to false when we have all the retry code
            })
        }
    }

    /// Step G (device is cross-signed by the sender)
    fn device_is_cross_signed_by_sender(&self, sender_device: Device) -> OlmResult<SenderData> {
        // H (cross-signing key matches that used to sign the device!)
        let user_id = sender_device.user_id().to_owned();

        let master_key = sender_device
            .device_owner_identity
            .as_ref()
            .and_then(|i| i.master_key().get_first_key());

        if let Some(master_key) = master_key {
            // We have user_id and master_key for the user sending the to-device message.
            let master_key_verified = sender_device.is_cross_signing_trusted();
            Ok(SenderData::SenderKnown { user_id, master_key, master_key_verified })
        } else {
            // Surprisingly, there was no key in the MasterPubkey. We did not expect this:
            // treat it as if the device was not signed by this master key.
            //
            error!("MasterPubkey for user {user_id} does not contain any keys!",);

            Ok(SenderData::DeviceInfo {
                device_keys: sender_device.as_device_keys().clone(),
                retry_details: SenderDataRetryDetails::retry_soon(),
                legacy_session: true, // TODO: change to false when retries etc. are done
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{ops::Deref as _, sync::Arc};

    use assert_matches2::assert_let;
    use matrix_sdk_test::async_test;
    use ruma::{device_id, owned_room_id, user_id, DeviceId, OwnedUserId, UserId};
    use tokio::sync::Mutex;
    use vodozemac::{megolm::SessionKey, Curve25519PublicKey, Ed25519PublicKey};

    use super::SenderDataFinder;
    use crate::{
        olm::{PrivateCrossSigningIdentity, SenderData},
        store::{Changes, CryptoStoreWrapper, MemoryStore, Store},
        types::events::{
            olm_v1::DecryptedRoomKeyEvent,
            room_key::{MegolmV1AesSha2Content, RoomKeyContent},
        },
        verification::VerificationMachine,
        Account, Device, DeviceData, OtherUserIdentityData, OwnUserIdentityData, UserIdentityData,
    };

    impl<'a> SenderDataFinder<'a> {
        fn new(store: &'a Store) -> Self {
            Self { store }
        }
    }

    #[async_test]
    async fn test_providing_no_device_data_returns_sender_data_with_no_device_info() {
        // Given that the device is not in the store and the initial event has no device
        // info
        let setup = TestSetup::new(TestOptions {
            store_contains_device: false,
            store_contains_sender_identity: false,
            device_is_signed: true,
            event_contains_device_keys: false,
            sender_is_ourself: false,
            sender_is_verified: false,
        })
        .await;
        let finder = SenderDataFinder::new(&setup.store);

        // When we try to find sender data
        let sender_data = finder
            .have_event(setup.sender_device_curve_key(), &setup.room_key_event)
            .await
            .unwrap();

        // Then we get back no useful information at all
        assert_let!(SenderData::UnknownDevice { retry_details, legacy_session } = sender_data);
        assert_eq!(retry_details.retry_count, 0);

        // TODO: This should not be marked as a legacy session, but for now it is
        // because we haven't finished implementing the whole sender_data and
        // retry mechanism.
        assert!(legacy_session);
    }

    #[async_test]
    async fn test_if_the_todevice_event_contains_device_info_it_is_captured() {
        // Given that the signed device keys are in the event
        let setup = TestSetup::new(TestOptions {
            store_contains_device: false,
            store_contains_sender_identity: false,
            device_is_signed: true,
            event_contains_device_keys: true,
            sender_is_ourself: false,
            sender_is_verified: false,
        })
        .await;
        let finder = SenderDataFinder::new(&setup.store);

        // When we try to find sender data
        let sender_data = finder
            .have_event(setup.sender_device_curve_key(), &setup.room_key_event)
            .await
            .unwrap();

        // Then we get back the device keys that were in the event
        assert_let!(
            SenderData::DeviceInfo { device_keys, retry_details, legacy_session } = sender_data
        );
        assert_eq!(&device_keys, setup.sender_device.as_device_keys());
        assert_eq!(retry_details.retry_count, 0);

        // TODO: This should not be marked as a legacy session, but for now it is
        // because we haven't finished implementing the whole sender_data and
        // retry mechanism.
        assert!(legacy_session);
    }

    #[async_test]
    async fn test_picks_up_device_info_from_the_store_if_missing_from_the_todevice_event() {
        // Given that the device keys are not in the event but the device is in the
        // store
        let setup = TestSetup::new(TestOptions {
            store_contains_device: true,
            store_contains_sender_identity: false,
            device_is_signed: true,
            event_contains_device_keys: false,
            sender_is_ourself: false,
            sender_is_verified: false,
        })
        .await;
        let finder = SenderDataFinder::new(&setup.store);

        // When we try to find sender data
        let sender_data = finder
            .have_event(setup.sender_device_curve_key(), &setup.room_key_event)
            .await
            .unwrap();

        // Then we get back the device keys that were in the store
        assert_let!(
            SenderData::DeviceInfo { device_keys, retry_details, legacy_session } = sender_data
        );
        assert_eq!(&device_keys, setup.sender_device.as_device_keys());
        assert_eq!(retry_details.retry_count, 0);

        // TODO: This should not be marked as a legacy session, but for now it is
        // because we haven't finished implementing the whole sender_data and
        // retry mechanism.
        assert!(legacy_session);
    }

    #[async_test]
    async fn test_adds_device_info_even_if_it_is_not_signed() {
        // Given that the the device is in the store
        // But it is not signed
        let setup = TestSetup::new(TestOptions {
            store_contains_device: true,
            store_contains_sender_identity: false,
            device_is_signed: false,
            event_contains_device_keys: false,
            sender_is_ourself: false,
            sender_is_verified: false,
        })
        .await;
        let finder = SenderDataFinder::new(&setup.store);

        // When we try to find sender data
        let sender_data = finder
            .have_event(setup.sender_device_curve_key(), &setup.room_key_event)
            .await
            .unwrap();

        // Then we store the device info even though it is useless, in case we want to
        // check it matches up later.
        assert_let!(
            SenderData::DeviceInfo { device_keys, retry_details, legacy_session } = sender_data
        );
        assert_eq!(&device_keys, setup.sender_device.as_device_keys());
        assert_eq!(retry_details.retry_count, 0);

        // TODO: This should not be marked as a legacy session, but for now it is
        // because we haven't finished implementing the whole sender_data and
        // retry mechanism.
        assert!(legacy_session);
    }

    #[async_test]
    async fn test_adds_sender_data_for_own_verified_device_and_user_using_device_from_store() {
        // Given the device is in the store, and we sent the event
        let setup = TestSetup::new(TestOptions {
            store_contains_device: true,
            store_contains_sender_identity: true,
            device_is_signed: true,
            event_contains_device_keys: false,
            sender_is_ourself: true,
            sender_is_verified: false,
        })
        .await;
        let finder = SenderDataFinder::new(&setup.store);

        // When we try to find sender data
        let sender_data = finder
            .have_event(setup.sender_device_curve_key(), &setup.room_key_event)
            .await
            .unwrap();

        // Then we get back the information about the sender
        assert_let!(
            SenderData::SenderKnown { user_id, master_key, master_key_verified } = sender_data
        );
        assert_eq!(user_id, setup.sender.user_id);
        assert_eq!(master_key, setup.sender_master_key());
        assert!(!master_key_verified);
    }

    #[async_test]
    async fn test_adds_sender_data_for_other_verified_device_and_user_using_device_from_store() {
        // Given the device is in the store, and someone else sent the event
        let setup = TestSetup::new(TestOptions {
            store_contains_device: true,
            store_contains_sender_identity: true,
            device_is_signed: true,
            event_contains_device_keys: false,
            sender_is_ourself: false,
            sender_is_verified: false,
        })
        .await;
        let finder = SenderDataFinder::new(&setup.store);

        // When we try to find sender data
        let sender_data = finder
            .have_event(setup.sender_device_curve_key(), &setup.room_key_event)
            .await
            .unwrap();

        // Then we get back the information about the sender
        assert_let!(
            SenderData::SenderKnown { user_id, master_key, master_key_verified } = sender_data
        );
        assert_eq!(user_id, setup.sender.user_id);
        assert_eq!(master_key, setup.sender_master_key());
        assert!(!master_key_verified);
    }

    #[async_test]
    async fn test_adds_sender_data_for_own_device_and_user_using_device_from_event() {
        // Given the device keys are in the event, and we sent the event
        let setup = TestSetup::new(TestOptions {
            store_contains_device: false,
            store_contains_sender_identity: true,
            device_is_signed: true,
            event_contains_device_keys: true,
            sender_is_ourself: true,
            sender_is_verified: false,
        })
        .await;
        let finder = SenderDataFinder::new(&setup.store);

        // When we try to find sender data
        let sender_data = finder
            .have_event(setup.sender_device_curve_key(), &setup.room_key_event)
            .await
            .unwrap();

        // Then we get back the information about the sender
        assert_let!(
            SenderData::SenderKnown { user_id, master_key, master_key_verified } = sender_data
        );
        assert_eq!(user_id, setup.sender.user_id);
        assert_eq!(master_key, setup.sender_master_key());
        assert!(!master_key_verified);
    }

    #[async_test]
    async fn test_adds_sender_data_for_other_verified_device_and_user_using_device_from_event() {
        // Given the device keys are in the event, and someone else sent the event
        let setup = TestSetup::new(TestOptions {
            store_contains_device: false,
            store_contains_sender_identity: true,
            device_is_signed: true,
            event_contains_device_keys: true,
            sender_is_ourself: false,
            sender_is_verified: false,
        })
        .await;
        let finder = SenderDataFinder::new(&setup.store);

        // When we try to find sender data
        let sender_data = finder
            .have_event(setup.sender_device_curve_key(), &setup.room_key_event)
            .await
            .unwrap();

        // Then we get back the information about the sender
        assert_let!(
            SenderData::SenderKnown { user_id, master_key, master_key_verified } = sender_data
        );
        assert_eq!(user_id, setup.sender.user_id);
        assert_eq!(master_key, setup.sender_master_key());
        assert!(!master_key_verified);
    }

    #[async_test]
    async fn test_notes_master_key_is_verified_for_own_identity() {
        // Given we can find the device, and we sent the event, and we are verified
        let setup = TestSetup::new(TestOptions {
            store_contains_device: true,
            store_contains_sender_identity: true,
            device_is_signed: true,
            event_contains_device_keys: true,
            sender_is_ourself: true,
            sender_is_verified: true,
        })
        .await;
        let finder = SenderDataFinder::new(&setup.store);

        // When we try to find sender data
        let sender_data = finder
            .have_event(setup.sender_device_curve_key(), &setup.room_key_event)
            .await
            .unwrap();

        // Then we get back the information about the sender
        assert_let!(
            SenderData::SenderKnown { user_id, master_key, master_key_verified } = sender_data
        );
        assert_eq!(user_id, setup.sender.user_id);
        assert_eq!(master_key, setup.sender_master_key());
        // Including the fact that it was verified
        assert!(master_key_verified);
    }

    #[async_test]
    async fn test_notes_master_key_is_verified_for_other_identity() {
        // Given we can find the device, and someone else sent the event
        // And the sender is verified
        let setup = TestSetup::new(TestOptions {
            store_contains_device: true,
            store_contains_sender_identity: true,
            device_is_signed: true,
            event_contains_device_keys: true,
            sender_is_ourself: false,
            sender_is_verified: true,
        })
        .await;
        let finder = SenderDataFinder::new(&setup.store);

        // When we try to find sender data
        let sender_data = finder
            .have_event(setup.sender_device_curve_key(), &setup.room_key_event)
            .await
            .unwrap();

        // Then we get back the information about the sender
        assert_let!(
            SenderData::SenderKnown { user_id, master_key, master_key_verified } = sender_data
        );
        assert_eq!(user_id, setup.sender.user_id);
        assert_eq!(master_key, setup.sender_master_key());
        // Including the fact that it was verified
        assert!(master_key_verified);
    }

    #[async_test]
    async fn test_can_add_user_sender_data_based_on_a_provided_device() {
        // Given the device is not in the store or the event
        let setup = TestSetup::new(TestOptions {
            store_contains_device: false,
            store_contains_sender_identity: true,
            device_is_signed: true,
            event_contains_device_keys: false,
            sender_is_ourself: false,
            sender_is_verified: false,
        })
        .await;
        let finder = SenderDataFinder::new(&setup.store);

        // When we supply the device keys directly while asking for the sender data
        let sender_data =
            finder.have_device_keys(setup.sender_device.as_device_keys()).await.unwrap();

        // Then it is found using the device we supplied
        assert_let!(
            SenderData::SenderKnown { user_id, master_key, master_key_verified } = sender_data
        );
        assert_eq!(user_id, setup.sender.user_id);
        assert_eq!(master_key, setup.sender_master_key());
        assert!(!master_key_verified);
    }

    struct TestOptions {
        store_contains_device: bool,
        store_contains_sender_identity: bool,
        device_is_signed: bool,
        event_contains_device_keys: bool,
        sender_is_ourself: bool,
        sender_is_verified: bool,
    }

    struct TestSetup {
        sender: TestUser,
        sender_device: Device,
        store: Store,
        room_key_event: DecryptedRoomKeyEvent,
    }

    impl TestSetup {
        async fn new(options: TestOptions) -> Self {
            let me = TestUser::own().await;
            let sender = TestUser::other(&me, &options).await;

            let sender_device = if options.device_is_signed {
                create_signed_device(&sender.account, &*sender.private_identity.lock().await).await
            } else {
                create_unsigned_device(&sender.account)
            };

            let store = create_store(&me);

            save_to_store(&store, &me, &sender, &sender_device, &options).await;

            let room_key_event =
                create_room_key_event(&sender.user_id, &me.user_id, &sender_device, &options);

            Self { sender, sender_device, store, room_key_event }
        }

        fn sender_device_curve_key(&self) -> Curve25519PublicKey {
            self.sender_device.curve25519_key().unwrap()
        }

        fn sender_master_key(&self) -> Ed25519PublicKey {
            self.sender.user_identity.master_key().get_first_key().unwrap()
        }
    }

    fn create_store(me: &TestUser) -> Store {
        let store_wrapper = Arc::new(CryptoStoreWrapper::new(&me.user_id, MemoryStore::new()));

        let verification_machine = VerificationMachine::new(
            me.account.deref().clone(),
            Arc::clone(&me.private_identity),
            Arc::clone(&store_wrapper),
        );

        Store::new(
            me.account.static_data.clone(),
            Arc::clone(&me.private_identity),
            store_wrapper,
            verification_machine,
        )
    }

    async fn save_to_store(
        store: &Store,
        me: &TestUser,
        sender: &TestUser,
        sender_device: &Device,
        options: &TestOptions,
    ) {
        let mut changes = Changes::default();

        // If the device should exist in the store, add it
        if options.store_contains_device {
            changes.devices.new.push(sender_device.inner.clone())
        }

        // Add the sender identity to the store
        if options.store_contains_sender_identity {
            changes.identities.new.push(sender.user_identity.clone());
        }

        // If it's different from the sender, add our identity too
        if !options.sender_is_ourself {
            changes.identities.new.push(me.user_identity.clone());
        }

        store.save_changes(changes).await.unwrap();
    }

    struct TestUser {
        user_id: OwnedUserId,
        account: Account,
        private_identity: Arc<Mutex<PrivateCrossSigningIdentity>>,
        user_identity: UserIdentityData,
    }

    impl TestUser {
        async fn new(
            user_id: &UserId,
            device_id: &DeviceId,
            is_me: bool,
            is_verified: bool,
            signer: Option<&TestUser>,
        ) -> Self {
            let account = Account::with_device_id(user_id, device_id);
            let user_id = user_id.to_owned();
            let private_identity = Arc::new(Mutex::new(create_private_identity(&account).await));

            let user_identity =
                create_user_identity(&*private_identity.lock().await, is_me, is_verified, signer)
                    .await;

            Self { user_id, account, private_identity, user_identity }
        }

        async fn own() -> Self {
            Self::new(user_id!("@myself:s.co"), device_id!("OWNDEVICEID"), true, true, None).await
        }

        async fn other(me: &TestUser, options: &TestOptions) -> Self {
            let user_id =
                if options.sender_is_ourself { &me.user_id } else { user_id!("@other:s.co") };

            Self::new(
                user_id,
                device_id!("SENDERDEVICEID"),
                options.sender_is_ourself,
                options.sender_is_verified,
                Some(me),
            )
            .await
        }
    }

    async fn create_user_identity(
        private_identity: &PrivateCrossSigningIdentity,
        is_me: bool,
        is_verified: bool,
        signer: Option<&TestUser>,
    ) -> UserIdentityData {
        if is_me {
            let own_user_identity = OwnUserIdentityData::from_private(private_identity).await;

            if is_verified {
                own_user_identity.mark_as_verified();
            }

            UserIdentityData::Own(own_user_identity)
        } else {
            let mut other_user_identity =
                OtherUserIdentityData::from_private(private_identity).await;

            if is_verified {
                sign_other_identity(signer, &mut other_user_identity).await;
            }

            UserIdentityData::Other(other_user_identity)
        }
    }

    async fn sign_other_identity(
        signer: Option<&TestUser>,
        other_user_identity: &mut OtherUserIdentityData,
    ) {
        if let Some(signer) = signer {
            let signer_private_identity = signer.private_identity.lock().await;

            let user_signing = signer_private_identity.user_signing_key.lock().await;

            let user_signing = user_signing.as_ref().unwrap();
            let master = user_signing.sign_user(&*other_user_identity).unwrap();
            other_user_identity.master_key = Arc::new(master.try_into().unwrap());

            user_signing.public_key().verify_master_key(other_user_identity.master_key()).unwrap();
        } else {
            panic!("You must provide a `signer` if you want an Other to be verified!");
        }
    }

    async fn create_private_identity(account: &Account) -> PrivateCrossSigningIdentity {
        PrivateCrossSigningIdentity::with_account(account).await.0
    }

    async fn create_signed_device(
        account: &Account,
        private_identity: &PrivateCrossSigningIdentity,
    ) -> Device {
        let mut read_only_device = DeviceData::from_account(account);

        let self_signing = private_identity.self_signing_key.lock().await;
        let self_signing = self_signing.as_ref().unwrap();

        let mut device_keys = read_only_device.as_device_keys().to_owned();
        self_signing.sign_device(&mut device_keys).unwrap();
        read_only_device.update_device(&device_keys).unwrap();

        wrap_device(account, read_only_device)
    }

    fn create_unsigned_device(account: &Account) -> Device {
        wrap_device(account, DeviceData::from_account(account))
    }

    fn wrap_device(account: &Account, read_only_device: DeviceData) -> Device {
        Device {
            inner: read_only_device,
            verification_machine: VerificationMachine::new(
                account.deref().clone(),
                Arc::new(Mutex::new(PrivateCrossSigningIdentity::new(
                    account.user_id().to_owned(),
                ))),
                Arc::new(CryptoStoreWrapper::new(account.user_id(), MemoryStore::new())),
            ),
            own_identity: None,
            device_owner_identity: None,
        }
    }

    fn create_room_key_event(
        sender: &UserId,
        receiver: &UserId,
        sender_device: &Device,
        options: &TestOptions,
    ) -> DecryptedRoomKeyEvent {
        let device = if options.event_contains_device_keys {
            Some(sender_device.as_device_keys().clone())
        } else {
            None
        };

        DecryptedRoomKeyEvent::new(
            sender,
            receiver,
            Ed25519PublicKey::from_base64("loz5i40dP+azDtWvsD0L/xpnCjNkmrcvtXVXzCHX8Vw").unwrap(),
            device,
            RoomKeyContent::MegolmV1AesSha2(Box::new(room_key_content())),
        )
    }

    fn room_key_content() -> MegolmV1AesSha2Content {
        MegolmV1AesSha2Content::new(
            owned_room_id!("!r:s.co"),
            "mysession".to_owned(),
            SessionKey::from_base64(
                "\
                    AgAAAADBy9+YIYTIqBjFT67nyi31gIOypZQl8day2hkhRDCZaHoG+cZh4tZLQIAZimJail0\
                    0zq4DVJVljO6cZ2t8kIto/QVk+7p20Fcf2nvqZyL2ZCda2Ei7VsqWZHTM/gqa2IU9+ktkwz\
                    +KFhENnHvDhG9f+hjsAPZd5mTTpdO+tVcqtdWhX4dymaJ/2UpAAjuPXQW+nXhQWQhXgXOUa\
                    JCYurJtvbCbqZGeDMmVIoqukBs2KugNJ6j5WlTPoeFnMl6Guy9uH2iWWxGg8ZgT2xspqVl5\
                    CwujjC+m7Dh1toVkvu+bAw\
                    ",
            )
            .unwrap(),
        )
    }
}
