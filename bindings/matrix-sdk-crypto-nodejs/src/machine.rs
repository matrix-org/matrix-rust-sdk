//! The crypto specific Olm objects.

use std::{
    collections::{BTreeMap, HashMap},
    mem::ManuallyDrop,
    ops::Deref,
    sync::Arc,
};

use napi::bindgen_prelude::{within_runtime_if_available, Either7, FromNapiValue, ToNapiValue};
use napi_derive::*;
use ruma::{serde::Raw, DeviceKeyAlgorithm, OwnedTransactionId, UInt};
use serde_json::{value::RawValue, Value as JsonValue};
use zeroize::Zeroize;

use crate::{
    encryption, identifiers, into_err, olm, requests, responses, responses::response_from_string,
    sync_events, types, vodozemac,
};

/// The value used by the `OlmMachine` JS class.
///
/// It has 2 states: `Opened` and `Closed`. Why maintaining the state here?
/// Because NodeJS has no way to drop an object explicitly, and we want to be
/// able to “close” the `OlmMachine` to free all associated data. More over,
/// `napi-rs` doesn't allow a function to take the ownership of the type itself
/// (`fn close(self) { … }`). So we manage the state ourselves.
///
/// Using the `OlmMachine` when its state is `Closed` will panic.
enum OlmMachineInner {
    Opened(ManuallyDrop<matrix_sdk_crypto::OlmMachine>),
    Closed,
}

impl Drop for OlmMachineInner {
    fn drop(&mut self) {
        if let Self::Opened(machine) = self {
            // SAFETY: `self` won't be used anymore after this `take`, so it's safe to do it
            // here.
            let machine = unsafe { ManuallyDrop::take(machine) };
            within_runtime_if_available(move || drop(machine));
        }
    }
}

impl Deref for OlmMachineInner {
    type Target = matrix_sdk_crypto::OlmMachine;

    #[inline]
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Opened(machine) => machine,
            Self::Closed => panic!("The `OlmMachine` has been closed, cannot use it anymore"),
        }
    }
}

/// Represents the type of store an `OlmMachine` can use.
#[derive(Default)]
#[napi]
pub enum StoreType {
    /// Use `matrix-sdk-sled`.
    #[default]
    Sled,

    /// Use `matrix-sdk-sqlite`.
    Sqlite,
}

/// State machine implementation of the Olm/Megolm encryption protocol
/// used for Matrix end to end encryption.
// #[napi(custom_finalize)]
#[napi]
pub struct OlmMachine {
    inner: OlmMachineInner,
}

#[napi]
impl OlmMachine {
    // JavaScript doesn't support asynchronous constructor. So let's
    // use a factory pattern, where the constructor cannot be used (it
    // returns an error), and a new method is provided to construct
    // the object. napi provides `#[napi(factory)]` to address those
    // needs automatically. Unfortunately, it doesn't support
    // asynchronous factory methods.
    //
    // So let's do this manually. The `initialize` async method _is_
    // the factory function. We also manually implement the
    // constructor to raise an error when called.

    /// Create a new `OlmMachine` asynchronously.
    ///
    /// The persistence of the encryption keys and all the inner
    /// objects are controlled by the `store_path` argument.
    ///
    /// # Arguments
    ///
    /// * `user_id`, the unique ID of the user that owns this machine.
    /// * `device_id`, the unique id of the device that owns this machine.
    /// * `store_path`, the path to a directory where the state of the machine
    ///   should be persisted; if not set, the created machine will keep the
    ///   encryption keys only in memory, and once the object is dropped, the
    ///   keys will be lost.
    /// * `store_passphrase`, the passphrase that should be used to encrypt the
    ///   data at rest in the store. **Warning**, if no passphrase is given, the
    ///   store and all its data will remain unencrypted. This argument is
    ///   ignored if `store_path` is not set.
    #[napi(strict)]
    pub async fn initialize(
        user_id: &identifiers::UserId,
        device_id: &identifiers::DeviceId,
        store_path: Option<String>,
        mut store_passphrase: Option<String>,
        store_type: Option<StoreType>,
    ) -> napi::Result<OlmMachine> {
        let user_id = user_id.clone().inner;
        let device_id = device_id.clone().inner;

        let user_id = user_id.as_ref();
        let device_id = device_id.as_ref();

        Ok(OlmMachine {
            inner: OlmMachineInner::Opened(ManuallyDrop::new(match store_path {
                Some(store_path) => {
                    let machine = match store_type.unwrap_or_default() {
                        StoreType::Sled => {
                            matrix_sdk_crypto::OlmMachine::with_store(
                                user_id,
                                device_id,
                                matrix_sdk_sled::SledCryptoStore::open(
                                    store_path,
                                    store_passphrase.as_deref(),
                                )
                                .await
                                .map(Arc::new)
                                .map_err(into_err)?,
                            )
                            .await
                        }

                        StoreType::Sqlite => {
                            matrix_sdk_crypto::OlmMachine::with_store(
                                user_id,
                                device_id,
                                matrix_sdk_sqlite::SqliteCryptoStore::open(
                                    store_path,
                                    store_passphrase.as_deref(),
                                )
                                .await
                                .map(Arc::new)
                                .map_err(into_err)?,
                            )
                            .await
                        }
                    };

                    store_passphrase.zeroize();

                    machine.map_err(into_err)?
                }

                None => matrix_sdk_crypto::OlmMachine::new(user_id, device_id).await,
            })),
        })
    }

    /// It's not possible to construct an `OlmMachine` with its
    /// constructor because building an `OlmMachine` is
    /// asynchronous. Please use the `finalize` method.
    #[napi(constructor)]
    pub fn new() -> napi::Result<Self> {
        Err(napi::Error::from_reason(
            "To build an `OldMachine`, please use the `initialize` method",
        ))
    }

    /// The unique user ID that owns this `OlmMachine` instance.
    #[napi(getter)]
    pub fn user_id(&self) -> identifiers::UserId {
        identifiers::UserId::from(self.inner.user_id().to_owned())
    }

    /// The unique device ID that identifies this `OlmMachine`.
    #[napi(getter)]
    pub fn device_id(&self) -> identifiers::DeviceId {
        identifiers::DeviceId::from(self.inner.device_id().to_owned())
    }

    /// Get the public parts of our Olm identity keys.
    #[napi(getter)]
    pub fn identity_keys(&self) -> vodozemac::IdentityKeys {
        self.inner.identity_keys().into()
    }

    /// Handle a to-device and one-time key counts from a sync response.
    ///
    /// This will decrypt and handle to-device events returning the
    /// decrypted versions of them, as a JSON-encoded string.
    ///
    /// To decrypt an event from the room timeline, please use
    /// `decrypt_room_event`.
    ///
    /// # Arguments
    ///
    /// * `to_device_events`, the to-device events of the current sync response.
    /// * `changed_devices`, the list of devices that changed in this sync
    ///   response.
    /// * `one_time_keys_count`, the current one-time keys counts that the sync
    ///   response returned.
    #[napi(strict)]
    pub async fn receive_sync_changes(
        &self,
        to_device_events: String,
        changed_devices: &sync_events::DeviceLists,
        one_time_key_counts: HashMap<String, u32>,
        unused_fallback_keys: Vec<String>,
    ) -> napi::Result<String> {
        let to_device_events = serde_json::from_str(to_device_events.as_ref()).map_err(into_err)?;
        let changed_devices = changed_devices.inner.clone();
        let one_time_key_counts = one_time_key_counts
            .iter()
            .map(|(key, value)| (DeviceKeyAlgorithm::from(key.as_str()), UInt::from(*value)))
            .collect::<BTreeMap<_, _>>();
        let unused_fallback_keys = Some(
            unused_fallback_keys
                .into_iter()
                .map(|key| DeviceKeyAlgorithm::from(key.as_str()))
                .collect::<Vec<_>>(),
        );

        serde_json::to_string(
            &self
                .inner
                .receive_sync_changes(
                    to_device_events,
                    &changed_devices,
                    &one_time_key_counts,
                    unused_fallback_keys.as_deref(),
                )
                .await
                .map_err(into_err)?,
        )
        .map_err(into_err)
    }

    /// Get the outgoing requests that need to be sent out.
    ///
    /// This returns a list of `KeysUploadRequest`, or
    /// `KeysQueryRequest`, or `KeysClaimRequest`, or
    /// `ToDeviceRequest`, or `SignatureUploadRequest`, or
    /// `RoomMessageRequest`, or `KeysBackupRequest`. Those requests
    /// need to be sent out to the server and the responses need to be
    /// passed back to the state machine using `mark_request_as_sent`.
    #[napi]
    pub async fn outgoing_requests(
        &self,
    ) -> napi::Result<
        Vec<
            // We could be tempted to use `requests::OutgoingRequests` as its
            // a type alias for this giant `Either7`. But `napi` won't unfold
            // it properly into a valid TypeScript definition, so…  let's
            // copy-paste :-(.
            Either7<
                requests::KeysUploadRequest,
                requests::KeysQueryRequest,
                requests::KeysClaimRequest,
                requests::ToDeviceRequest,
                requests::SignatureUploadRequest,
                requests::RoomMessageRequest,
                requests::KeysBackupRequest,
            >,
        >,
    > {
        self.inner
            .outgoing_requests()
            .await
            .map_err(into_err)?
            .into_iter()
            .map(requests::OutgoingRequest)
            .map(TryFrom::try_from)
            .collect()
    }

    /// Mark the request with the given request ID as sent.
    ///
    /// # Arguments
    ///
    /// * `request_id`, the unique ID of the request that was sent out. This is
    ///   needed to couple the response with the now sent out request.
    /// * `request_type`, the request type associated to the request ID.
    /// * `response`, the response that was received from the server after the
    ///   outgoing request was sent out.
    #[napi(strict)]
    pub async fn mark_request_as_sent(
        &self,
        request_id: String,
        request_type: requests::RequestType,
        response: String,
    ) -> napi::Result<bool> {
        let transaction_id = OwnedTransactionId::from(request_id);
        let response = response_from_string(response.as_str()).map_err(into_err)?;
        let incoming_response = responses::OwnedResponse::try_from((request_type, response))?;

        self.inner
            .mark_request_as_sent(&transaction_id, &incoming_response)
            .await
            .map(|_| true)
            .map_err(into_err)
    }

    /// Get the a key claiming request for the user/device pairs that
    /// we are missing Olm sessions for.
    ///
    /// Returns `null` if no key claiming request needs to be sent
    /// out.
    ///
    /// Sessions need to be established between devices so group
    /// sessions for a room can be shared with them.
    ///
    /// This should be called every time a group session needs to be
    /// shared as well as between sync calls. After a sync some
    /// devices may request room keys without us having a valid Olm
    /// session with them, making it impossible to server the room key
    /// request, thus it’s necessary to check for missing sessions
    /// between sync as well.
    ///
    /// Note: Care should be taken that only one such request at a
    /// time is in flight, e.g. using a lock.
    ///
    /// The response of a successful key claiming requests needs to be
    /// passed to the `OlmMachine` with the `mark_request_as_sent`.
    ///
    /// # Arguments
    ///
    /// * `users`, the list of users that we should check if we lack a session
    ///   with one of their devices. This can be an empty array or `null` when
    ///   calling this method between sync requests.
    #[napi(strict)]
    pub async fn get_missing_sessions(
        &self,
        users: Option<Vec<&identifiers::UserId>>,
    ) -> napi::Result<Option<requests::KeysClaimRequest>> {
        let users = users
            .unwrap_or_default()
            .into_iter()
            .map(|user| user.inner.clone())
            .collect::<Vec<_>>();

        match self
            .inner
            .get_missing_sessions(users.iter().map(AsRef::as_ref))
            .await
            .map_err(into_err)?
        {
            Some((transaction_id, keys_claim_request)) => Ok(Some(
                requests::KeysClaimRequest::try_from((
                    transaction_id.to_string(),
                    &keys_claim_request,
                ))
                .map_err(into_err)?,
            )),

            None => Ok(None),
        }
    }

    /// Update the tracked users.
    ///
    /// This will mark users that weren’t seen before for a key query
    /// and tracking.
    ///
    /// If the user is already known to the Olm machine it will not be
    /// considered for a key query.
    ///
    /// # Arguments
    ///
    /// * `users`, an array over user IDs that should be marked for tracking.
    #[napi(strict)]
    pub async fn update_tracked_users(&self, users: Vec<&identifiers::UserId>) -> napi::Result<()> {
        let users = users.into_iter().map(|user| user.inner.clone()).collect::<Vec<_>>();

        self.inner.update_tracked_users(users.iter().map(AsRef::as_ref)).await.map_err(into_err)?;

        Ok(())
    }

    /// Get to-device requests to share a room key with users in a room.
    ///
    /// # Arguments
    ///
    /// * `room_id`, the room ID of the room where the room key will be used.
    /// * `users`, the list of users that should receive the room key.
    /// * `encryption_settings`, the encryption settings.
    #[napi(strict)]
    pub async fn share_room_key(
        &self,
        room_id: &identifiers::RoomId,
        users: Vec<&identifiers::UserId>,
        encryption_settings: &encryption::EncryptionSettings,
    ) -> napi::Result<String> {
        let room_id = room_id.inner.clone();
        let users = users.into_iter().map(|user| user.inner.clone()).collect::<Vec<_>>();
        let encryption_settings =
            matrix_sdk_crypto::olm::EncryptionSettings::from(encryption_settings);

        serde_json::to_string(
            &self
                .inner
                .share_room_key(&room_id, users.iter().map(AsRef::as_ref), encryption_settings)
                .await
                .map_err(into_err)?,
        )
        .map_err(into_err)
    }

    /// Encrypt a JSON-encoded content for the given room.
    ///
    /// # Arguments
    ///
    /// * `room_id`, the ID of the room for which the message should be
    ///   encrypted.
    /// * `event_type`, the plaintext type of the event.
    /// * `content`, the JSON-encoded content of the message that should be
    ///   encrypted.
    #[napi(strict)]
    pub async fn encrypt_room_event(
        &self,
        room_id: &identifiers::RoomId,
        event_type: String,
        content: String,
    ) -> napi::Result<String> {
        let room_id = room_id.inner.clone();
        let content: JsonValue = serde_json::from_str(content.as_str()).map_err(into_err)?;

        serde_json::to_string(
            &self
                .inner
                .encrypt_room_event_raw(&room_id, content, event_type.as_ref())
                .await
                .map_err(into_err)?,
        )
        .map_err(into_err)
    }

    /// Decrypt an event from a room timeline.
    ///
    /// # Arguments
    ///
    /// * `event`, the event that should be decrypted.
    /// * `room_id`, the ID of the room where the event was sent to.
    #[napi(strict)]
    pub async fn decrypt_room_event(
        &self,
        event: String,
        room_id: &identifiers::RoomId,
    ) -> napi::Result<responses::DecryptedRoomEvent> {
        let event = Raw::from_json(RawValue::from_string(event).map_err(into_err)?);
        let room_id = room_id.inner.clone();

        let room_event = self.inner.decrypt_room_event(&event, &room_id).await.map_err(into_err)?;

        Ok(room_event.into())
    }

    /// Get the status of the private cross signing keys.
    ///
    /// This can be used to check which private cross signing keys we
    /// have stored locally.
    #[napi]
    pub async fn cross_signing_status(&self) -> olm::CrossSigningStatus {
        self.inner.cross_signing_status().await.into()
    }

    /// Sign the given message using our device key and if available
    /// cross-signing master key.
    #[napi(strict)]
    pub async fn sign(&self, message: String) -> types::Signatures {
        self.inner.sign(message.as_str()).await.into()
    }

    /// Shut down the `OlmMachine`.
    ///
    /// The `OlmMachine` cannot be used after this method has been called,
    /// otherwise it will panic.
    ///
    /// All associated resources will be closed too, like the crypto storage
    /// connections.
    ///
    /// # Safety
    ///
    /// The caller is responsible to **not** use any objects that came from this
    /// `OlmMachine` after this `close` method has been called.
    #[napi(strict)]
    pub fn close(&mut self) {
        self.inner = OlmMachineInner::Closed;
    }
}
