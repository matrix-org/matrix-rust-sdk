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
use vodozemac::Curve25519PublicKey;

use super::{InboundGroupSession, SenderData};
use crate::{
    CryptoStoreError, Device, DeviceData, MegolmError, OlmError, SignatureError,
    error::MismatchedIdentityKeysError, store::Store, types::events::olm_v1::DecryptedRoomKeyEvent,
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
/// ╱ Is the session owned by the device?                               ╲yes
/// ╲___________________________________________________________________╱ │
///                                     │ no                              │
///                                     ▼                                 │
/// ╭───────────────────────────────────────────────────────────────────╮ │
/// │ E (the device does not own the session)                           │ │
/// │                                                                   │ │
/// │ Give up: something is wrong with the session.                     │ │
/// ╰───────────────────────────────────────────────────────────────────╯ │
///                                     ┌─────────────────────────────────┘
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
    session: &'a InboundGroupSession,
}

impl<'a> SenderDataFinder<'a> {
    /// Find the device associated with the to-device message used to
    /// create the InboundGroupSession we are about to create, and decide
    /// whether we trust the sender.
    pub(crate) async fn find_using_event(
        store: &'a Store,
        sender_curve_key: Curve25519PublicKey,
        room_key_event: &'a DecryptedRoomKeyEvent,
        session: &'a InboundGroupSession,
    ) -> Result<SenderData, SessionDeviceKeysCheckError> {
        let finder = Self { store, session };
        finder.have_event(sender_curve_key, room_key_event).await
    }

    /// Use the supplied device data to decide whether we trust the sender.
    pub(crate) async fn find_using_device_data(
        store: &'a Store,
        device_data: DeviceData,
        session: &'a InboundGroupSession,
    ) -> Result<SenderData, SessionDeviceCheckError> {
        let finder = Self { store, session };
        finder.have_device_data(device_data).await
    }

    /// Find the device using the curve key provided, and decide whether we
    /// trust the sender.
    pub(crate) async fn find_using_curve_key(
        store: &'a Store,
        sender_curve_key: Curve25519PublicKey,
        sender_user_id: &'a UserId,
        session: &'a InboundGroupSession,
    ) -> Result<SenderData, SessionDeviceCheckError> {
        let finder = Self { store, session };
        finder.search_for_device(sender_curve_key, sender_user_id).await
    }

    /// Step A (start - we have a to-device message containing a room key)
    async fn have_event(
        &self,
        sender_curve_key: Curve25519PublicKey,
        room_key_event: &'a DecryptedRoomKeyEvent,
    ) -> Result<SenderData, SessionDeviceKeysCheckError> {
        // Does the to-device message contain the device_keys property from MSC4147?
        if let Some(sender_device_keys) = &room_key_event.sender_device_keys {
            // Yes: use the device keys to continue.

            // Validate the signature of the DeviceKeys supplied. (We've actually already
            // done this when decrypting the event, but doing it again here is
            // relatively harmless and is the easiest way of getting hold of a
            // DeviceData so that we can follow the rest of this logic).
            let sender_device_data = DeviceData::try_from(sender_device_keys)?;
            Ok(self.have_device_data(sender_device_data).await?)
        } else {
            // No: look for the device in the store
            Ok(self.search_for_device(sender_curve_key, &room_key_event.sender).await?)
        }
    }

    /// Step B (there are no device keys in the to-device message)
    async fn search_for_device(
        &self,
        sender_curve_key: Curve25519PublicKey,
        sender_user_id: &UserId,
    ) -> Result<SenderData, SessionDeviceCheckError> {
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
                // This is not a legacy session since we did attempt to look
                // up its sender data at the time of reception.
                legacy_session: false,
                owner_check_failed: false,
            };
            Ok(sender_data)
        }
    }

    async fn have_device_data(
        &self,
        sender_device_data: DeviceData,
    ) -> Result<SenderData, SessionDeviceCheckError> {
        let sender_device = self.store.wrap_device_data(sender_device_data).await?;
        self.have_device(sender_device)
    }

    /// Step D (we have a device)
    ///
    /// Returns Err if the device does not own the session.
    fn have_device(&self, sender_device: Device) -> Result<SenderData, SessionDeviceCheckError> {
        // Is the session owned by the device?
        let device_is_owner = sender_device.is_owner_of_session(self.session)?;

        if !device_is_owner {
            // Step E (the device does not own the session)
            // Give up: something is wrong with the session.
            Ok(SenderData::UnknownDevice { legacy_session: false, owner_check_failed: true })
        } else {
            // Steps F, G, and H: we have a device, which may or may not be signed by the
            // sender.
            Ok(SenderData::from_device(&sender_device))
        }
    }
}

#[derive(Debug)]
pub(crate) enum SessionDeviceCheckError {
    CryptoStoreError(CryptoStoreError),
    MismatchedIdentityKeys(MismatchedIdentityKeysError),
}

impl From<CryptoStoreError> for SessionDeviceCheckError {
    fn from(e: CryptoStoreError) -> Self {
        Self::CryptoStoreError(e)
    }
}

impl From<MismatchedIdentityKeysError> for SessionDeviceCheckError {
    fn from(e: MismatchedIdentityKeysError) -> Self {
        Self::MismatchedIdentityKeys(e)
    }
}

impl From<SessionDeviceCheckError> for OlmError {
    fn from(e: SessionDeviceCheckError) -> Self {
        match e {
            SessionDeviceCheckError::CryptoStoreError(e) => e.into(),
            SessionDeviceCheckError::MismatchedIdentityKeys(e) => {
                OlmError::SessionCreation(e.into())
            }
        }
    }
}

impl From<SessionDeviceCheckError> for MegolmError {
    fn from(e: SessionDeviceCheckError) -> Self {
        match e {
            SessionDeviceCheckError::CryptoStoreError(e) => e.into(),
            SessionDeviceCheckError::MismatchedIdentityKeys(e) => e.into(),
        }
    }
}

#[derive(Debug)]
pub(crate) enum SessionDeviceKeysCheckError {
    CryptoStoreError(CryptoStoreError),
    MismatchedIdentityKeys(MismatchedIdentityKeysError),
    SignatureError(SignatureError),
}

impl From<CryptoStoreError> for SessionDeviceKeysCheckError {
    fn from(e: CryptoStoreError) -> Self {
        Self::CryptoStoreError(e)
    }
}

impl From<MismatchedIdentityKeysError> for SessionDeviceKeysCheckError {
    fn from(e: MismatchedIdentityKeysError) -> Self {
        Self::MismatchedIdentityKeys(e)
    }
}

impl From<SignatureError> for SessionDeviceKeysCheckError {
    fn from(e: SignatureError) -> Self {
        Self::SignatureError(e)
    }
}

impl From<SessionDeviceCheckError> for SessionDeviceKeysCheckError {
    fn from(e: SessionDeviceCheckError) -> Self {
        match e {
            SessionDeviceCheckError::CryptoStoreError(e) => Self::CryptoStoreError(e),
            SessionDeviceCheckError::MismatchedIdentityKeys(e) => Self::MismatchedIdentityKeys(e),
        }
    }
}

impl From<SessionDeviceKeysCheckError> for OlmError {
    fn from(e: SessionDeviceKeysCheckError) -> Self {
        match e {
            SessionDeviceKeysCheckError::CryptoStoreError(e) => e.into(),
            SessionDeviceKeysCheckError::MismatchedIdentityKeys(e) => {
                OlmError::SessionCreation(e.into())
            }
            SessionDeviceKeysCheckError::SignatureError(e) => OlmError::SessionCreation(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{ops::Deref as _, sync::Arc};

    use assert_matches2::assert_let;
    use matrix_sdk_test::async_test;
    use ruma::{DeviceId, OwnedUserId, RoomId, UserId, device_id, room_id, user_id};
    use tokio::sync::Mutex;
    use vodozemac::{Curve25519PublicKey, Ed25519PublicKey, megolm::SessionKey};

    use super::SenderDataFinder;
    use crate::{
        Account, Device, OtherUserIdentityData, OwnUserIdentityData, UserIdentityData,
        error::MismatchedIdentityKeysError,
        machine::test_helpers::{
            create_signed_device_of_unverified_user, create_unsigned_device,
            sign_user_identity_data,
        },
        olm::{
            InboundGroupSession, KnownSenderData, PrivateCrossSigningIdentity, SenderData,
            group_sessions::sender_data_finder::SessionDeviceKeysCheckError,
        },
        store::{CryptoStoreWrapper, MemoryStore, Store, types::Changes},
        types::{
            EventEncryptionAlgorithm,
            events::{
                olm_v1::DecryptedRoomKeyEvent,
                room_key::{MegolmV1AesSha2Content, RoomKeyContent},
            },
        },
        verification::VerificationMachine,
    };

    impl<'a> SenderDataFinder<'a> {
        fn new(store: &'a Store, session: &'a InboundGroupSession) -> Self {
            Self { store, session }
        }
    }

    #[async_test]
    async fn test_providing_no_device_data_returns_sender_data_with_no_device_info() {
        // Given that the device is not in the store and the initial event has no device
        // info
        let setup = TestSetup::new(TestOptions::new()).await;
        let finder = SenderDataFinder::new(&setup.store, &setup.session);

        // When we try to find sender data
        let sender_data = finder
            .have_event(setup.sender_device_curve_key(), &setup.room_key_event)
            .await
            .unwrap();

        // Then we get back no useful information at all
        assert_let!(SenderData::UnknownDevice { legacy_session, owner_check_failed } = sender_data);

        assert!(!legacy_session);
        assert!(!owner_check_failed);
    }

    #[async_test]
    async fn test_if_the_todevice_event_contains_device_info_it_is_captured() {
        // Given that the signed device keys are in the event
        let setup =
            TestSetup::new(TestOptions::new().device_is_signed().event_contains_device_keys())
                .await;
        let finder = SenderDataFinder::new(&setup.store, &setup.session);

        // When we try to find sender data
        let sender_data = finder
            .have_event(setup.sender_device_curve_key(), &setup.room_key_event)
            .await
            .unwrap();

        // Then we get back the device keys that were in the event
        assert_let!(SenderData::DeviceInfo { device_keys, legacy_session } = sender_data);
        assert_eq!(&device_keys, setup.sender_device.as_device_keys());
        assert!(!legacy_session);
    }

    #[async_test]
    async fn test_picks_up_device_info_from_the_store_if_missing_from_the_todevice_event() {
        // Given that the device keys are not in the event but the device is in the
        // store
        let setup =
            TestSetup::new(TestOptions::new().store_contains_device().device_is_signed()).await;
        let finder = SenderDataFinder::new(&setup.store, &setup.session);

        // When we try to find sender data
        let sender_data = finder
            .have_event(setup.sender_device_curve_key(), &setup.room_key_event)
            .await
            .unwrap();

        // Then we get back the device keys that were in the store
        assert_let!(SenderData::DeviceInfo { device_keys, legacy_session } = sender_data);
        assert_eq!(&device_keys, setup.sender_device.as_device_keys());
        assert!(!legacy_session);
    }

    #[async_test]
    async fn test_adds_device_info_even_if_it_is_not_signed() {
        // Given that the the device is in the store
        // But it is not signed
        let setup = TestSetup::new(TestOptions::new().store_contains_device()).await;
        let finder = SenderDataFinder::new(&setup.store, &setup.session);

        // When we try to find sender data
        let sender_data = finder
            .have_event(setup.sender_device_curve_key(), &setup.room_key_event)
            .await
            .unwrap();

        // Then we store the device info even though it is useless, in case we want to
        // check it matches up later.
        assert_let!(SenderData::DeviceInfo { device_keys, legacy_session } = sender_data);
        assert_eq!(&device_keys, setup.sender_device.as_device_keys());
        assert!(!legacy_session);
    }

    #[async_test]
    async fn test_adds_sender_data_for_own_verified_device_and_user_using_device_from_store() {
        // Given the device is in the store, and we sent the event
        let setup = TestSetup::new(
            TestOptions::new()
                .store_contains_device()
                .store_contains_sender_identity()
                .device_is_signed()
                .sender_is_ourself(),
        )
        .await;
        let finder = SenderDataFinder::new(&setup.store, &setup.session);

        // When we try to find sender data
        let sender_data = finder
            .have_event(setup.sender_device_curve_key(), &setup.room_key_event)
            .await
            .unwrap();

        // Then we get back the information about the sender
        assert_let!(
            SenderData::SenderUnverified(KnownSenderData { user_id, device_id, master_key }) =
                sender_data
        );
        assert_eq!(user_id, setup.sender.user_id);
        assert_eq!(device_id.unwrap(), setup.sender_device.device_id());
        assert_eq!(*master_key, setup.sender_master_key());
    }

    #[async_test]
    async fn test_adds_sender_data_for_other_verified_device_and_user_using_device_from_store() {
        // Given the device is in the store, and someone else sent the event
        let setup = TestSetup::new(
            TestOptions::new()
                .store_contains_device()
                .store_contains_sender_identity()
                .device_is_signed(),
        )
        .await;
        let finder = SenderDataFinder::new(&setup.store, &setup.session);

        // When we try to find sender data
        let sender_data = finder
            .have_event(setup.sender_device_curve_key(), &setup.room_key_event)
            .await
            .unwrap();

        // Then we get back the information about the sender
        assert_let!(
            SenderData::SenderUnverified(KnownSenderData { user_id, device_id, master_key }) =
                sender_data
        );
        assert_eq!(user_id, setup.sender.user_id);
        assert_eq!(device_id.unwrap(), setup.sender_device.device_id());
        assert_eq!(*master_key, setup.sender_master_key());
    }

    #[async_test]
    async fn test_adds_sender_data_for_own_device_and_user_using_device_from_event() {
        // Given the device keys are in the event, and we sent the event
        let setup = TestSetup::new(
            TestOptions::new()
                .store_contains_sender_identity()
                .device_is_signed()
                .event_contains_device_keys()
                .sender_is_ourself(),
        )
        .await;
        let finder = SenderDataFinder::new(&setup.store, &setup.session);

        // When we try to find sender data
        let sender_data = finder
            .have_event(setup.sender_device_curve_key(), &setup.room_key_event)
            .await
            .unwrap();

        // Then we get back the information about the sender
        assert_let!(
            SenderData::SenderUnverified(KnownSenderData { user_id, device_id, master_key }) =
                sender_data
        );
        assert_eq!(user_id, setup.sender.user_id);
        assert_eq!(device_id.unwrap(), setup.sender_device.device_id());
        assert_eq!(*master_key, setup.sender_master_key());
    }

    #[async_test]
    async fn test_adds_sender_data_for_other_verified_device_and_user_using_device_from_event() {
        // Given the device keys are in the event, and someone else sent the event
        let setup = TestSetup::new(
            TestOptions::new()
                .store_contains_sender_identity()
                .device_is_signed()
                .event_contains_device_keys(),
        )
        .await;
        let finder = SenderDataFinder::new(&setup.store, &setup.session);

        // When we try to find sender data
        let sender_data = finder
            .have_event(setup.sender_device_curve_key(), &setup.room_key_event)
            .await
            .unwrap();

        // Then we get back the information about the sender
        assert_let!(
            SenderData::SenderUnverified(KnownSenderData { user_id, device_id, master_key }) =
                sender_data
        );
        assert_eq!(user_id, setup.sender.user_id);
        assert_eq!(device_id.unwrap(), setup.sender_device.device_id());
        assert_eq!(*master_key, setup.sender_master_key());
    }

    #[async_test]
    async fn test_if_session_signing_does_not_match_device_return_an_error() {
        // Given everything is the same as the above test
        // except the session is not owned by the device
        let setup = TestSetup::new(
            TestOptions::new()
                .store_contains_sender_identity()
                .device_is_signed()
                .event_contains_device_keys()
                .session_signing_key_differs_from_device(),
        )
        .await;
        let finder = SenderDataFinder::new(&setup.store, &setup.session);

        // When we try to find sender data
        assert_let!(
            Err(e) =
                finder.have_event(setup.sender_device_curve_key(), &setup.room_key_event).await
        );

        assert_let!(SessionDeviceKeysCheckError::MismatchedIdentityKeys(e) = e);

        let key_ed25519 =
            Box::new(setup.session.signing_keys().iter().next().unwrap().1.ed25519().unwrap());
        let key_curve25519 = Box::new(setup.session.sender_key());

        let device_ed25519 = setup.sender_device.ed25519_key().map(Box::new);
        let device_curve25519 = Some(Box::new(setup.sender_device_curve_key()));

        assert_eq!(
            e,
            MismatchedIdentityKeysError {
                key_ed25519,
                device_ed25519,
                key_curve25519,
                device_curve25519
            }
        );
    }

    #[async_test]
    async fn test_does_not_add_sender_data_for_a_device_missing_keys() {
        // Given everything is the same as the successful test
        // except the device does not own the session because
        // it is imported.
        let setup = TestSetup::new(
            TestOptions::new()
                .store_contains_sender_identity()
                .session_is_imported()
                .event_contains_device_keys(),
        )
        .await;
        let finder = SenderDataFinder::new(&setup.store, &setup.session);

        // When we try to find sender data
        let sender_data = finder
            .have_event(setup.sender_device_curve_key(), &setup.room_key_event)
            .await
            .unwrap();

        // Then we fail to find useful sender data
        assert_let!(SenderData::UnknownDevice { legacy_session, owner_check_failed } = sender_data);
        assert!(!legacy_session);

        // And report that the owner_check_failed
        assert!(owner_check_failed);
    }

    #[async_test]
    async fn test_notes_master_key_is_verified_for_own_identity() {
        // Given we can find the device, and we sent the event, and we are verified
        let setup = TestSetup::new(
            TestOptions::new()
                .store_contains_device()
                .store_contains_sender_identity()
                .device_is_signed()
                .event_contains_device_keys()
                .sender_is_ourself()
                .sender_is_verified(),
        )
        .await;
        let finder = SenderDataFinder::new(&setup.store, &setup.session);

        // When we try to find sender data
        let sender_data = finder
            .have_event(setup.sender_device_curve_key(), &setup.room_key_event)
            .await
            .unwrap();

        // Then we get back the information about the sender
        assert_let!(
            SenderData::SenderVerified(KnownSenderData { user_id, device_id, master_key }) =
                sender_data
        );
        assert_eq!(user_id, setup.sender.user_id);
        assert_eq!(device_id.unwrap(), setup.sender_device.device_id());
        assert_eq!(*master_key, setup.sender_master_key());
    }

    #[async_test]
    async fn test_notes_master_key_is_verified_for_other_identity() {
        // Given we can find the device, and someone else sent the event
        // And the sender is verified
        let setup = TestSetup::new(
            TestOptions::new()
                .store_contains_device()
                .store_contains_sender_identity()
                .device_is_signed()
                .event_contains_device_keys()
                .sender_is_verified(),
        )
        .await;
        let finder = SenderDataFinder::new(&setup.store, &setup.session);

        // When we try to find sender data
        let sender_data = finder
            .have_event(setup.sender_device_curve_key(), &setup.room_key_event)
            .await
            .unwrap();

        // Then we get back the information about the sender
        assert_let!(
            SenderData::SenderVerified(KnownSenderData { user_id, device_id, master_key }) =
                sender_data
        );
        assert_eq!(user_id, setup.sender.user_id);
        assert_eq!(device_id.unwrap(), setup.sender_device.device_id());
        assert_eq!(*master_key, setup.sender_master_key());
    }

    #[async_test]
    async fn test_can_add_user_sender_data_based_on_a_provided_device() {
        // Given the device is not in the store or the event
        let setup =
            TestSetup::new(TestOptions::new().store_contains_sender_identity().device_is_signed())
                .await;
        let finder = SenderDataFinder::new(&setup.store, &setup.session);

        // When we supply the device keys directly while asking for the sender data
        let sender_data = finder.have_device_data(setup.sender_device.inner.clone()).await.unwrap();

        // Then it is found using the device we supplied
        assert_let!(
            SenderData::SenderUnverified(KnownSenderData { user_id, device_id, master_key }) =
                sender_data
        );
        assert_eq!(user_id, setup.sender.user_id);
        assert_eq!(device_id.unwrap(), setup.sender_device.device_id());
        assert_eq!(*master_key, setup.sender_master_key());
    }

    struct TestOptions {
        store_contains_device: bool,
        store_contains_sender_identity: bool,
        device_is_signed: bool,
        event_contains_device_keys: bool,
        sender_is_ourself: bool,
        sender_is_verified: bool,
        session_signing_key_differs_from_device: bool,
        session_is_imported: bool,
    }

    impl TestOptions {
        fn new() -> Self {
            Self {
                store_contains_device: false,
                store_contains_sender_identity: false,
                device_is_signed: false,
                event_contains_device_keys: false,
                sender_is_ourself: false,
                sender_is_verified: false,
                session_signing_key_differs_from_device: false,
                session_is_imported: false,
            }
        }

        fn store_contains_device(mut self) -> Self {
            self.store_contains_device = true;
            self
        }

        fn store_contains_sender_identity(mut self) -> Self {
            self.store_contains_sender_identity = true;
            self
        }

        fn device_is_signed(mut self) -> Self {
            self.device_is_signed = true;
            self
        }

        fn event_contains_device_keys(mut self) -> Self {
            self.event_contains_device_keys = true;
            self
        }

        fn sender_is_ourself(mut self) -> Self {
            self.sender_is_ourself = true;
            self
        }

        fn sender_is_verified(mut self) -> Self {
            self.sender_is_verified = true;
            self
        }

        fn session_signing_key_differs_from_device(mut self) -> Self {
            self.session_signing_key_differs_from_device = true;
            self
        }

        fn session_is_imported(mut self) -> Self {
            self.session_is_imported = true;
            self
        }
    }

    struct TestSetup {
        sender: TestUser,
        sender_device: Device,
        store: Store,
        room_key_event: DecryptedRoomKeyEvent,
        session: InboundGroupSession,
    }

    impl TestSetup {
        async fn new(options: TestOptions) -> Self {
            let me = TestUser::own().await;
            let sender = TestUser::other(&me, &options).await;

            let sender_device = if options.device_is_signed {
                create_signed_device_of_unverified_user(
                    sender.account.device_keys(),
                    &*sender.private_identity.lock().await,
                )
                .await
            } else {
                create_unsigned_device(sender.account.device_keys())
            };

            let store = create_store(&me);

            save_to_store(&store, &me, &sender, &sender_device, &options).await;

            let room_id = room_id!("!r:s.co");
            let session_key = create_session_key();

            let room_key_event = create_room_key_event(
                &sender.user_id,
                &me.user_id,
                &sender_device,
                room_id,
                &session_key,
                &options,
            );

            let signing_key = if options.session_signing_key_differs_from_device {
                Ed25519PublicKey::from_base64("2/5LWJMow5zhJqakV88SIc7q/1pa8fmkfgAzx72w9G4")
                    .unwrap()
            } else {
                sender_device.inner.ed25519_key().unwrap()
            };

            let mut session = InboundGroupSession::new(
                sender_device.inner.curve25519_key().unwrap(),
                signing_key,
                room_id,
                &session_key,
                SenderData::unknown(),
                None,
                EventEncryptionAlgorithm::MegolmV1AesSha2,
                None,
                false,
            )
            .unwrap();
            if options.session_is_imported {
                session.mark_as_imported();
            }

            Self { sender, sender_device, store, room_key_event, session }
        }

        fn sender_device_curve_key(&self) -> Curve25519PublicKey {
            self.sender_device.curve25519_key().unwrap()
        }

        fn sender_master_key(&self) -> Ed25519PublicKey {
            self.sender.user_identity.master_key().get_first_key().unwrap()
        }
    }

    fn create_store(me: &TestUser) -> Store {
        let store_wrapper = Arc::new(CryptoStoreWrapper::new(
            &me.user_id,
            me.account.device_id(),
            MemoryStore::new(),
        ));

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
            let private_identity =
                Arc::new(Mutex::new(PrivateCrossSigningIdentity::for_account(&account)));

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
            sign_user_identity_data(signer_private_identity.deref(), other_user_identity).await;
        } else {
            panic!("You must provide a `signer` if you want an Other to be verified!");
        }
    }

    fn create_room_key_event(
        sender: &UserId,
        receiver: &UserId,
        sender_device: &Device,
        room_id: &RoomId,
        session_key: &SessionKey,
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
            RoomKeyContent::MegolmV1AesSha2(Box::new(MegolmV1AesSha2Content::new(
                room_id.to_owned(),
                "mysession".to_owned(),
                clone_session_key(session_key),
                false,
            ))),
        )
    }

    fn create_session_key() -> SessionKey {
        SessionKey::from_base64(
            "\
            AgAAAADBy9+YIYTIqBjFT67nyi31gIOypZQl8day2hkhRDCZaHoG+cZh4tZLQIAZimJail0\
            0zq4DVJVljO6cZ2t8kIto/QVk+7p20Fcf2nvqZyL2ZCda2Ei7VsqWZHTM/gqa2IU9+ktkwz\
            +KFhENnHvDhG9f+hjsAPZd5mTTpdO+tVcqtdWhX4dymaJ/2UpAAjuPXQW+nXhQWQhXgXOUa\
            JCYurJtvbCbqZGeDMmVIoqukBs2KugNJ6j5WlTPoeFnMl6Guy9uH2iWWxGg8ZgT2xspqVl5\
            CwujjC+m7Dh1toVkvu+bAw\
            ",
        )
        .unwrap()
    }

    fn clone_session_key(session_key: &SessionKey) -> SessionKey {
        SessionKey::from_base64(&session_key.to_base64()).unwrap()
    }
}
