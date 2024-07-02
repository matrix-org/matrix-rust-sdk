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

use std::ops::Deref;

use async_trait::async_trait;
use ruma::UserId;
use tracing::error;
use vodozemac::Curve25519PublicKey;

use super::{SenderData, SenderDataRetryDetails};
use crate::{
    error::OlmResult,
    identities::Device,
    store::{self, Store},
    types::{events::olm_v1::DecryptedRoomKeyEvent, DeviceKeys, MasterPubkey},
    EventError, OlmError, OlmMachine, ReadOnlyDevice, ReadOnlyOwnUserIdentity,
    ReadOnlyUserIdentities,
};

/// Temporary struct that is used to look up [`SenderData`] based on the
/// information supplied with in [`InboundGroupSession`].
///
/// The letters A, B etc. in the documentation refer to the algorithm described
/// in https://github.com/matrix-org/matrix-rust-sdk/issues/3543
pub(crate) struct SenderDataFinder<'a, S: FinderCryptoStore> {
    own_crypto_store: &'a S,
    own_user_id: &'a UserId,
}

impl<'a> SenderDataFinder<'a, Store> {
    /// As described in https://github.com/matrix-org/matrix-rust-sdk/issues/3543
    /// and https://github.com/matrix-org/matrix-rust-sdk/issues/3544
    /// find the device info associated with the to-device message used to
    /// create the InboundGroupSession we are about to create, and decide
    /// whether we trust the sender.
    pub(crate) async fn find_using_event(
        own_olm_machine: &'a OlmMachine,
        sender_curve_key: Curve25519PublicKey,
        room_key_event: &'a DecryptedRoomKeyEvent,
    ) -> OlmResult<SenderData> {
        let finder = Self {
            own_crypto_store: own_olm_machine.store(),
            own_user_id: own_olm_machine.user_id(),
        };
        finder.have_event(sender_curve_key, room_key_event).await
    }

    /// As described in https://github.com/matrix-org/matrix-rust-sdk/issues/3543
    /// and https://github.com/matrix-org/matrix-rust-sdk/issues/3544
    /// use the supplied device info to decide whether we trust the sender.
    pub(crate) async fn find_using_device_keys(
        own_olm_machine: &'a OlmMachine,
        device_keys: DeviceKeys,
    ) -> OlmResult<SenderData> {
        let finder = Self {
            own_crypto_store: own_olm_machine.store(),
            own_user_id: own_olm_machine.user_id(),
        };
        finder.have_device_keys(&device_keys).await
    }
}

impl<'a, S: FinderCryptoStore> SenderDataFinder<'a, S> {
    async fn have_event(
        &self,
        sender_curve_key: Curve25519PublicKey,
        room_key_event: &'a DecryptedRoomKeyEvent,
    ) -> OlmResult<SenderData> {
        // A (start)
        //
        // TODO: take the session lock for session_id
        // TODO: if we fail to get the lock, we bail out immediately. Does this open an
        // attack vector for someone to use someone else's session ID and send
        // invalid sessions?
        //
        // Does the to-device message contain the device_keys property from MSC4147?
        if let Some(sender_device_keys) = &room_key_event.device_keys {
            // Yes: use the device info to continue
            self.have_device_keys(sender_device_keys).await
        } else {
            // No: look for the device in the store
            self.search_for_device(sender_curve_key, room_key_event).await
        }
    }

    async fn search_for_device(
        &self,
        sender_curve_key: Curve25519PublicKey,
        room_key_event: &'a DecryptedRoomKeyEvent,
    ) -> OlmResult<SenderData> {
        // B (no device info in to-device message)
        //
        // Does the locally-cached (in the store) devices list contain a device with the
        // curve key of the sender of the to-device message?
        if let Some(sender_device) = self
            .own_crypto_store
            .get_device_from_curve_key(&room_key_event.sender, sender_curve_key)
            .await?
        {
            // Yes: use the device info to continue
            self.have_device(sender_device.inner).await
        } else {
            // C (no device info locally)
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
            // sender_data will be persisted to the store when this function returns to
            // `handle_key`, meaning that if our process is killed, we will still retry it
            // later.
            //
            // Switch to the "slow lane" (don't block sync, but retry after /keys/query
            // returns).
            //
            // TODO: kick off an async task [keep the lock]: run
            // OlmMachine::get_user_devices (which waits for /keys/query to complete, then
            // repeats the lookup we did above for the device that matches this session.
            //
            // If the device is there, -> D
            //
            // If we still don’t have the device info, -> Wait to see whether we get device
            // info later. Increment retry_count and set next_retry_time_ms per backoff
            // algorithm; let the background job pick it up [drop the lock]
            Ok(sender_data)
        }
    }

    async fn have_device_keys(&self, sender_device_keys: &DeviceKeys) -> OlmResult<SenderData> {
        // Validate the signature of the DeviceKeys supplied.
        if let Ok(sender_device) = ReadOnlyDevice::try_from(sender_device_keys) {
            self.have_device(sender_device).await
        } else {
            // The device keys supplied did not validate.
            // TODO: log an error
            // TODO: Err(OlmError::SessionCreation(SessionCreationError::InvalidDeviceKeys))
            Err(OlmError::EventError(EventError::UnsupportedAlgorithm))
        }
    }

    /// Step D from https://github.com/matrix-org/matrix-rust-sdk/issues/3543
    /// We have device info for the sender of this to-device message. Look up
    /// whether it's cross-signed.
    async fn have_device(&self, sender_device: ReadOnlyDevice) -> OlmResult<SenderData> {
        // D (we have device info)
        //
        // Is the device info cross-signed?

        let user_id = sender_device.user_id();
        let Some(signatures) = sender_device.signatures().get(user_id) else {
            // This should never happen: we would not have managed to get a ReadOnlyDevice
            // if it did not contain a signature.
            error!(
                "Found a device for user_id {user_id} but it has no signatures for that user id!"
            );

            // Return the same result as if the device is not cross-signed.
            return Ok(SenderData::unknown());
        };

        // Count number of signatures - we know there is 1, because we would not have
        // been able to construct a ReadOnlyDevice without a signature.
        // If there are more than 1, we assume this device was cross-signed by some
        // identity.
        if signatures.len() > 1 {
            // Yes, the device info is cross-signed
            self.device_is_cross_signed(sender_device).await
        } else {
            // No, the device info is not cross-signed.
            // Wait to see whether the device becomes cross-signed later. Drop
            // out of both the "fast lane" and the "slow lane" and let the
            // background retry task try this later.
            //
            // We will need new, cross-signed device info for this to work, so there is no
            // point storing the device info we have in the session.
            Ok(SenderData::unknown())

            // TODO: Wait to see if the device becomes cross-signed soon.
            // Increment retry_count and set next_retry_time_ms per
            // backoff algorithm; let the background job pick it up [drop
            // the lock]
        }
    }

    async fn device_is_cross_signed(&self, sender_device: ReadOnlyDevice) -> OlmResult<SenderData> {
        // E (we have cross-signed device info)
        //
        // Do we have the cross-signing key for this user?

        let sender_user_id = sender_device.user_id();
        let sender_user_identity = self.own_crypto_store.get_user_identity(sender_user_id).await?;

        if let Some(sender_user_identity) = sender_user_identity {
            // Yes: check the device is signed by the identity
            self.have_user_cross_signing_keys(sender_device, sender_user_identity).await
        } else {
            // No: F (we have cross-signed device info, but no cross-signing keys)

            // TODO: bump the retry count + time

            Ok(SenderData::DeviceInfo {
                device_keys: sender_device.as_device_keys().clone(),
                retry_details: SenderDataRetryDetails::retry_soon(),
                legacy_session: true, // TODO: change to false when we have all the retry code
            })

            // TODO: Return, and kick off an async task
            // [keep the lock]: run OlmMachine::get_identity (which waits for
            // /keys/query to complete, then fetches this user's cross-signing
            // key from the store.) If we still don’t have a cross-signing key
            // ->  Wait to see if we get one soon. Do nothing; let the
            // background job pick it up [drop the lock]
        }
    }

    async fn have_user_cross_signing_keys(
        &self,
        sender_device: ReadOnlyDevice,
        sender_user_identity: ReadOnlyUserIdentities,
    ) -> OlmResult<SenderData> {
        // G (we have cross-signing key)
        //
        // Does the cross-signing key match that used to sign the device info?
        // And is the signature in the device info valid?
        let maybe_msk_info =
            self.msk_if_device_is_signed_by_user(&sender_device, &sender_user_identity).await;

        if let Some((msk, msk_verified)) = maybe_msk_info {
            // Yes: H (cross-signing key matches that used to sign the device info!)
            // and: J (device info is verified by matching cross-signing key)

            // Find the actual key within the MasterPubkey struct
            if let Some(msk) = msk.get_first_key() {
                // We have MXID and MSK for the user sending the to-device message.
                // Decide the MSK trust level based on whether we have verified this user.
                // Set the MXID, MSK and trust level in the session. Remove the device
                // info and retries since we don't need them.
                Ok(SenderData::SenderKnown {
                    user_id: sender_device.user_id().to_owned(),
                    msk,
                    msk_verified,
                })
                // TODO: [drop the lock]
            } else {
                // Surprisingly, there was no key in the MasterPubkey. We did not expect this:
                // treat it as if the device was not signed by this master key.
                //
                tracing::error!(
                    "MasterPubkey for user {} does not contain any keys!",
                    msk.user_id()
                );

                Ok(SenderData::DeviceInfo {
                    device_keys: sender_device.as_device_keys().clone(),
                    retry_details: SenderDataRetryDetails::retry_soon(),
                    legacy_session: true, // TODO: change to false when retries etc. are done
                })
                // TODO: [drop the lock]
            }
        } else {
            // No: Device was not signed by the known identity of the sender.
            // (Or the signature was invalid. We don't know which unfortunately, so we can't
            // bail out completely if the signature was invalid, as we'd like
            // to.)
            //
            // Since we've already checked there are >1 signatures, we guess
            // it was signed by a different identity of this user, so we should retry
            // later, in case either the device info or the user's identity changes.
            Ok(SenderData::DeviceInfo {
                device_keys: sender_device.as_device_keys().clone(),
                retry_details: SenderDataRetryDetails::retry_soon(),
                legacy_session: true, // TODO: change to false when retries etc. are done
            })

            // TODO: Increment retry_count and set
            // next_retry_time_ms per backoff algorithm; let the background job
            // pick it up [drop the lock]
        }
    }

    /// If sender_device is correctly cross-signed by user_identity, return
    /// user_identity's master key and whether it is verified.
    /// Otherwise, return None.
    async fn msk_if_device_is_signed_by_user<'i>(
        &self,
        sender_device: &'_ ReadOnlyDevice,
        sender_user_identity: &'i ReadOnlyUserIdentities,
    ) -> Option<(&'i MasterPubkey, bool)> {
        match sender_user_identity {
            ReadOnlyUserIdentities::Own(own_identity) => {
                if own_identity.is_device_signed(sender_device).is_ok() {
                    Some((own_identity.master_key(), own_identity.is_verified()))
                } else {
                    None
                }
            }
            ReadOnlyUserIdentities::Other(other_identity) => {
                if other_identity.is_device_signed(sender_device).is_err() {
                    return None;
                }

                let msk = other_identity.master_key();

                // Use our own identity to determine whether this other identity is signed
                let msk_verified = if let Some(own_identity) = self.own_identity().await {
                    own_identity.is_identity_signed(other_identity).is_ok()
                } else {
                    // Couldn't get own identity! Assume msk is not verified.
                    false
                };

                Some((msk, msk_verified))
            }
        }
    }

    /// Return the user identity of the current user, or None if we failed to
    /// find it (which is unexpected)
    async fn own_identity(&self) -> Option<ReadOnlyOwnUserIdentity> {
        let own_identity =
            self.own_crypto_store.get_user_identity(self.own_user_id).await.ok()??;

        let ReadOnlyUserIdentities::Own(own_identity) = own_identity else {
            panic!("The user identity for our own user ID was not an own identity!");
        };

        Some(own_identity)
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub(crate) trait FinderCryptoStore {
    async fn get_device_from_curve_key(
        &self,
        user_id: &UserId,
        curve_key: Curve25519PublicKey,
    ) -> store::Result<Option<Device>>;

    async fn get_user_identity(
        &self,
        user_id: &UserId,
    ) -> OlmResult<Option<ReadOnlyUserIdentities>>;
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl FinderCryptoStore for Store {
    async fn get_device_from_curve_key(
        &self,
        user_id: &UserId,
        curve_key: Curve25519PublicKey,
    ) -> store::Result<Option<Device>> {
        self.get_device_from_curve_key(user_id, curve_key).await
    }

    async fn get_user_identity(
        &self,
        user_id: &UserId,
    ) -> OlmResult<Option<ReadOnlyUserIdentities>> {
        Ok(self.deref().get_user_identity(user_id).await?)
    }
}

#[cfg(test)]
mod tests {
    use std::{ops::Deref as _, sync::Arc};

    use assert_matches2::assert_let;
    use async_trait::async_trait;
    use matrix_sdk_test::async_test;
    use ruma::{device_id, owned_room_id, user_id, UserId};
    use tokio::sync::Mutex;
    use vodozemac::{megolm::SessionKey, Curve25519PublicKey, Ed25519PublicKey};

    use super::{FinderCryptoStore, SenderDataFinder};
    use crate::{
        error::OlmResult,
        olm::{PrivateCrossSigningIdentity, SenderData},
        store::{self, CryptoStoreWrapper, MemoryStore},
        types::events::{
            olm_v1::DecryptedRoomKeyEvent,
            room_key::{MegolmV1AesSha2Content, RoomKeyContent},
        },
        verification::VerificationMachine,
        Account, Device, ReadOnlyDevice, ReadOnlyOwnUserIdentity, ReadOnlyUserIdentities,
        ReadOnlyUserIdentity,
    };

    #[async_test]
    async fn test_providing_no_device_data_returns_sender_data_with_no_device_info() {
        // Given that the crypto store is empty and the initial event has no device info
        let room_key_content = room_key_content();
        let room_key_event = room_key_event(None, &room_key_content);
        let own_crypto_store = FakeCryptoStore::empty();

        // When we try to find sender data
        let finder = create_finder(&own_crypto_store, None);
        let sender_data = finder.have_event(create_curve_key(), &room_key_event).await.unwrap();

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
        // Given an account and user identity
        let account = Account::with_device_id(user_id!("@u:s.co"), device_id!("DEVICEID"));
        let private_identity = create_private_identity(&account).await;

        // And a device signed by the user identity
        let device = create_signed_device(&account, &private_identity).await;

        // And an event containing device info
        let room_key_content = room_key_content();
        let room_key_event = room_key_event(Some(&device), &room_key_content);

        // When we try to find sender data
        let own_crypto_store = FakeCryptoStore::empty();
        let finder = create_finder(&own_crypto_store, None);
        let sender_data = finder.have_event(create_curve_key(), &room_key_event).await.unwrap();

        // Then we get back the device keys that were in the store
        assert_let!(
            SenderData::DeviceInfo { device_keys, retry_details, legacy_session } = sender_data
        );
        assert_eq!(&device_keys, device.as_device_keys());
        assert_eq!(retry_details.retry_count, 0);

        // TODO: This should not be marked as a legacy session, but for now it is
        // because we haven't finished implementing the whole sender_data and
        // retry mechanism.
        assert!(legacy_session);
    }

    #[async_test]
    async fn test_picks_up_device_info_from_the_store_if_missing_from_the_todevice_event() {
        // Given an account and user identity
        let account = Account::with_device_id(user_id!("@u:s.co"), device_id!("DEVICEID"));
        let private_identity = create_private_identity(&account).await;

        // And a device signed by the user identity
        let device = create_signed_device(&account, &private_identity).await;

        // And an event (not containing device info)
        let room_key_content = room_key_content();
        let room_key_event = room_key_event(None, &room_key_content);

        // When we try to find sender data (but the user identity can't be found in the
        // store)
        let own_crypto_store = FakeCryptoStore::device_only(device.clone());
        let finder = create_finder(&own_crypto_store, None);
        let sender_data = finder.have_event(create_curve_key(), &room_key_event).await.unwrap();

        // Then we get back the device keys that were in the store
        assert_let!(
            SenderData::DeviceInfo { device_keys, retry_details, legacy_session } = sender_data
        );
        assert_eq!(&device_keys, device.as_device_keys());
        assert_eq!(retry_details.retry_count, 0);

        // TODO: This should not be marked as a legacy session, but for now it is
        // because we haven't finished implementing the whole sender_data and
        // retry mechanism.
        assert!(legacy_session);
    }

    #[async_test]
    async fn test_does_not_add_sender_data_if_device_is_not_signed() {
        // Given an account and user identity
        let account = Account::with_device_id(user_id!("@u:s.co"), device_id!("DEVICEID"));
        let private_identity = create_private_identity(&account).await;
        let user_identity = ReadOnlyOwnUserIdentity::from_private(&private_identity).await;
        let user_identities = ReadOnlyUserIdentities::Own(user_identity.clone());

        // And a device (not signed)
        let device = create_unsigned_device(&account);

        // And an event (not containing device info)
        let room_key_content = room_key_content();
        let room_key_event = room_key_event(None, &room_key_content);

        // When we try to find sender data
        let own_crypto_store = FakeCryptoStore::device_and_user(device, user_identities);
        let finder = create_finder(&own_crypto_store, None);
        let sender_data = finder.have_event(create_curve_key(), &room_key_event).await.unwrap();

        // Then we treat it as if there is no device info at all
        assert_let!(SenderData::UnknownDevice { retry_details, legacy_session } = sender_data);
        assert_eq!(retry_details.retry_count, 0);

        // TODO: This should not be marked as a legacy session, but for now it is
        // because we haven't finished implementing the whole sender_data and
        // retry mechanism.
        assert!(legacy_session);
    }

    #[async_test]
    async fn test_adds_sender_data_for_own_verified_device_and_user_using_device_from_store() {
        // Given an account and user identity
        let account = Account::with_device_id(user_id!("@u:s.co"), device_id!("DEVICEID"));
        let private_identity = create_private_identity(&account).await;
        let user_identity = ReadOnlyOwnUserIdentity::from_private(&private_identity).await;
        let user_identities = ReadOnlyUserIdentities::Own(user_identity.clone());

        // And a device signed by the user identity
        let device = create_signed_device(&account, &private_identity).await;

        // And an event (not containing device info)
        let room_key_content = room_key_content();
        let room_key_event = room_key_event(None, &room_key_content);

        // When we try to find sender data
        let own_crypto_store = FakeCryptoStore::device_and_user(device, user_identities);
        let finder = create_finder(&own_crypto_store, None);
        let sender_data = finder.have_event(create_curve_key(), &room_key_event).await.unwrap();

        // Then we get back the information about the sender
        assert_let!(SenderData::SenderKnown { user_id, msk, msk_verified } = sender_data);
        assert_eq!(user_id, account.user_id());
        assert_eq!(msk, user_identity.master_key().get_first_key().unwrap());
        assert!(!msk_verified);
    }

    #[async_test]
    async fn test_adds_sender_data_for_other_verified_device_and_user_using_device_from_store() {
        // Given an account, user identity, and a separate identity to be our own
        let account = Account::with_device_id(user_id!("@u:s.co"), device_id!("DEVICEID"));
        let private_identity = create_private_identity(&account).await;
        let user_identity = ReadOnlyUserIdentity::from_private(&private_identity).await;
        let user_identities = ReadOnlyUserIdentities::Other(user_identity.clone());
        let own_private_identity = create_private_identity(&account).await;
        let own_user_identity = ReadOnlyOwnUserIdentity::from_private(&own_private_identity).await;
        let own_user_identities = ReadOnlyUserIdentities::Own(own_user_identity.clone());

        // And a device signed by the user identity
        let device = create_signed_device(&account, &private_identity).await;

        // And an event (not containing device info)
        let room_key_content = room_key_content();
        let room_key_event = room_key_event(None, &room_key_content);

        // When we try to find sender data
        let own_crypto_store =
            FakeCryptoStore::new(Some(device), Some(user_identities), Some(own_user_identities));

        let finder = create_finder(&own_crypto_store, Some(user_id!("@myself:s.co")));
        let sender_data = finder.have_event(create_curve_key(), &room_key_event).await.unwrap();

        // Then we get back the information about the sender
        assert_let!(SenderData::SenderKnown { user_id, msk, msk_verified } = sender_data);
        assert_eq!(user_id, account.user_id());
        assert_eq!(msk, user_identity.master_key().get_first_key().unwrap());
        assert!(!msk_verified);
    }

    #[async_test]
    async fn test_adds_sender_data_for_own_verified_device_and_user_using_device_from_event() {
        // Given an account and user identity
        let account = Account::with_device_id(user_id!("@u:s.co"), device_id!("DEVICEID"));
        let private_identity = create_private_identity(&account).await;
        let user_identity = ReadOnlyOwnUserIdentity::from_private(&private_identity).await;
        let user_identities = ReadOnlyUserIdentities::Own(user_identity.clone());

        // And a device signed by the user identity
        let device = create_signed_device(&account, &private_identity).await;

        // And an event (not containing device info)
        let room_key_content = room_key_content();
        let room_key_event = room_key_event(Some(&device), &room_key_content);

        // When we try to find sender data
        let own_crypto_store = FakeCryptoStore::user_only(user_identities);
        let finder = create_finder(&own_crypto_store, None);
        let sender_data = finder.have_event(create_curve_key(), &room_key_event).await.unwrap();

        // Then we get back the information about the sender
        assert_let!(SenderData::SenderKnown { user_id, msk, msk_verified } = sender_data);
        assert_eq!(user_id, account.user_id());
        assert_eq!(msk, user_identity.master_key().get_first_key().unwrap());
        assert!(!msk_verified);
    }

    #[async_test]
    async fn test_adds_sender_data_for_other_verified_device_and_user_using_device_from_event() {
        // Given an account, user identity, and a separate identity to be our own
        let account = Account::with_device_id(user_id!("@u:s.co"), device_id!("DEVICEID"));
        let private_identity = create_private_identity(&account).await;
        let user_identity = ReadOnlyUserIdentity::from_private(&private_identity).await;
        let user_identities = ReadOnlyUserIdentities::Other(user_identity.clone());
        let own_private_identity = create_private_identity(&account).await;
        let own_user_identity = ReadOnlyOwnUserIdentity::from_private(&own_private_identity).await;
        let own_user_identities = ReadOnlyUserIdentities::Own(own_user_identity.clone());

        // And a device signed by the user identity
        let device = create_signed_device(&account, &private_identity).await;

        // And an event (not containing device info)
        let room_key_content = room_key_content();
        let room_key_event = room_key_event(Some(&device), &room_key_content);

        // When we try to find sender data
        let own_crypto_store =
            FakeCryptoStore::new(None, Some(user_identities), Some(own_user_identities));

        let finder = create_finder(&own_crypto_store, Some(user_id!("@myself:s.co")));
        let sender_data = finder.have_event(create_curve_key(), &room_key_event).await.unwrap();

        // Then we get back the information about the sender
        assert_let!(SenderData::SenderKnown { user_id, msk, msk_verified } = sender_data);
        assert_eq!(user_id, account.user_id());
        assert_eq!(msk, user_identity.master_key().get_first_key().unwrap());
        assert!(!msk_verified);
    }

    #[async_test]
    async fn test_marks_msk_as_verified_if_it_is_for_own_identity() {
        // Given an account and user identity which is verified
        let account = Account::with_device_id(user_id!("@u:s.co"), device_id!("DEVICEID"));
        let private_identity = create_private_identity(&account).await;
        let user_identity = ReadOnlyOwnUserIdentity::from_private(&private_identity).await;
        user_identity.mark_as_verified();
        let user_identities = ReadOnlyUserIdentities::Own(user_identity.clone());

        // And a device signed by the user identity
        let device = create_signed_device(&account, &private_identity).await;

        // And an event (not containing device info)
        let room_key_content = room_key_content();
        let room_key_event = room_key_event(Some(&device), &room_key_content);

        // When we try to find sender data
        let own_crypto_store = FakeCryptoStore::user_only(user_identities);
        let finder = create_finder(&own_crypto_store, None);
        let sender_data = finder.have_event(create_curve_key(), &room_key_event).await.unwrap();

        // Then we get back the information about the sender
        assert_let!(SenderData::SenderKnown { user_id, msk, msk_verified } = sender_data);
        assert_eq!(user_id, account.user_id());
        assert_eq!(msk, user_identity.master_key().get_first_key().unwrap());
        // Including the fact that it was verified
        assert!(msk_verified);
    }

    #[async_test]
    async fn test_marks_msk_as_verified_if_it_is_for_other_identity() {
        // Given an account, user identity, and a separate identity to be our own
        let account = Account::with_device_id(user_id!("@u:s.co"), device_id!("DEVICEID"));
        let private_identity = create_private_identity(&account).await;
        let mut user_identity = ReadOnlyUserIdentity::from_private(&private_identity).await;
        let own_private_identity = create_private_identity(&account).await;
        let own_user_identity = ReadOnlyOwnUserIdentity::from_private(&own_private_identity).await;

        // Where the other identity is verified (signed by our identity)
        {
            let user_signing = own_private_identity.user_signing_key.lock().await;
            let user_signing = user_signing.as_ref().unwrap();
            let master = user_signing.sign_user(&user_identity).unwrap();
            user_identity.master_key = Arc::new(master.try_into().unwrap());
            user_signing.public_key().verify_master_key(user_identity.master_key()).unwrap();
        }

        let own_user_identities = ReadOnlyUserIdentities::Own(own_user_identity.clone());
        let user_identities = ReadOnlyUserIdentities::Other(user_identity.clone());

        // And a device signed by the user identity
        let device = create_signed_device(&account, &private_identity).await;

        // And an event (not containing device info)
        let room_key_content = room_key_content();
        let room_key_event = room_key_event(Some(&device), &room_key_content);

        // When we try to find sender data
        let own_crypto_store =
            FakeCryptoStore::new(None, Some(user_identities), Some(own_user_identities));

        let finder = create_finder(&own_crypto_store, Some(user_id!("@myself:s.co")));
        let sender_data = finder.have_event(create_curve_key(), &room_key_event).await.unwrap();

        // Then we get back the information about the sender
        assert_let!(SenderData::SenderKnown { user_id, msk, msk_verified } = sender_data);
        assert_eq!(user_id, account.user_id());
        assert_eq!(msk, user_identity.master_key().get_first_key().unwrap());
        // Including the fact that it was verified
        assert!(msk_verified);
    }

    #[async_test]
    async fn test_adds_sender_data_based_on_existing_device() {
        // Given an account and user identity
        let account = Account::with_device_id(user_id!("@u:s.co"), device_id!("DEVICEID"));
        let private_identity = create_private_identity(&account).await;
        let user_identity = ReadOnlyOwnUserIdentity::from_private(&private_identity).await;
        let user_identities = ReadOnlyUserIdentities::Own(user_identity.clone());

        // And a device signed by the user identity
        let device = create_signed_device(&account, &private_identity).await;

        // When we try to find sender data directly using based in the device instead of
        // using a room key event
        let own_crypto_store = FakeCryptoStore::user_only(user_identities);
        let finder = create_finder(&own_crypto_store, None);
        let sender_data = finder.have_device_keys(device.as_device_keys()).await.unwrap();

        // Then we get back the information about the sender
        assert_let!(SenderData::SenderKnown { user_id, msk, msk_verified } = sender_data);
        assert_eq!(user_id, account.user_id());
        assert_eq!(msk, user_identity.master_key().get_first_key().unwrap());
        assert!(!msk_verified);
    }

    async fn create_private_identity(account: &Account) -> PrivateCrossSigningIdentity {
        PrivateCrossSigningIdentity::with_account(account).await.0
    }

    async fn create_signed_device(
        account: &Account,
        private_identity: &PrivateCrossSigningIdentity,
    ) -> Device {
        let mut read_only_device = ReadOnlyDevice::from_account(account);

        let self_signing = private_identity.self_signing_key.lock().await;
        let self_signing = self_signing.as_ref().unwrap();

        let mut device_keys = read_only_device.as_device_keys().to_owned();
        self_signing.sign_device(&mut device_keys).unwrap();
        read_only_device.update_device(&device_keys).unwrap();

        wrap_device(account, read_only_device)
    }

    fn create_unsigned_device(account: &Account) -> Device {
        wrap_device(account, ReadOnlyDevice::from_account(account))
    }

    fn wrap_device(account: &Account, read_only_device: ReadOnlyDevice) -> Device {
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

    fn create_curve_key() -> Curve25519PublicKey {
        Curve25519PublicKey::from_base64("7PUPP6Ijt5R8qLwK2c8uK5hqCNF9tOzWYgGaAay5JBs").unwrap()
    }

    fn create_finder<'a, S>(
        store: &'a S,
        own_user_id: Option<&'a UserId>,
    ) -> SenderDataFinder<'a, S>
    where
        S: FinderCryptoStore,
    {
        SenderDataFinder {
            own_crypto_store: store,
            own_user_id: own_user_id.unwrap_or(user_id!("@u:s.co")),
        }
    }

    fn room_key_event(
        device: Option<&Device>,
        content: &MegolmV1AesSha2Content,
    ) -> DecryptedRoomKeyEvent {
        DecryptedRoomKeyEvent::new(
            user_id!("@s:s.co"),
            user_id!("@u:s.co"),
            Ed25519PublicKey::from_base64("loz5i40dP+azDtWvsD0L/xpnCjNkmrcvtXVXzCHX8Vw").unwrap(),
            device.map(|d| d.as_device_keys().clone()),
            RoomKeyContent::MegolmV1AesSha2(Box::new(clone_content(content))),
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

    fn clone_content(content: &MegolmV1AesSha2Content) -> MegolmV1AesSha2Content {
        MegolmV1AesSha2Content::new(
            content.room_id.clone(),
            content.session_id.clone(),
            SessionKey::from_base64(&content.session_key.to_base64()).unwrap(),
        )
    }

    struct FakeCryptoStore {
        device: Option<Device>,
        user_identities: Option<ReadOnlyUserIdentities>,
        own_user_identities: Option<ReadOnlyUserIdentities>,
    }

    impl FakeCryptoStore {
        fn new(
            device: Option<Device>,
            user_identities: Option<ReadOnlyUserIdentities>,
            own_user_identities: Option<ReadOnlyUserIdentities>,
        ) -> Self {
            Self { device, user_identities, own_user_identities }
        }

        fn device_and_user(device: Device, user_identities: ReadOnlyUserIdentities) -> Self {
            Self {
                device: Some(device),
                user_identities: Some(user_identities),
                own_user_identities: None,
            }
        }

        fn device_only(device: Device) -> Self {
            Self { device: Some(device), user_identities: None, own_user_identities: None }
        }

        fn user_only(user_identities: ReadOnlyUserIdentities) -> Self {
            Self { device: None, user_identities: Some(user_identities), own_user_identities: None }
        }

        fn empty() -> Self {
            Self { device: None, user_identities: None, own_user_identities: None }
        }
    }

    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    impl FinderCryptoStore for FakeCryptoStore {
        async fn get_device_from_curve_key(
            &self,
            _user_id: &UserId,
            _curve_key: Curve25519PublicKey,
        ) -> store::Result<Option<Device>> {
            Ok(self.device.clone())
        }

        async fn get_user_identity(
            &self,
            user_id: &UserId,
        ) -> OlmResult<Option<ReadOnlyUserIdentities>> {
            if user_id == user_id!("@myself:s.co") {
                // We are being asked for our own user identity, different from
                // self.user_identities because that one was of Other type.
                Ok(self.own_user_identities.clone())
            } else {
                Ok(self.user_identities.clone())
            }
        }
    }
}
