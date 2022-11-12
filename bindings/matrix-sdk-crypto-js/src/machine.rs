//! The crypto specific Olm objects.

use std::collections::BTreeMap;

use js_sys::{Array, Function, Map, Promise, Set};
use ruma::{serde::Raw, DeviceKeyAlgorithm, OwnedTransactionId, UInt};
use serde_json::{json, Value as JsonValue};
use wasm_bindgen::prelude::*;

use crate::{
    device, encryption,
    future::future_to_promise,
    identifiers, identities,
    js::downcast,
    olm, requests,
    requests::OutgoingRequest,
    responses::{self, response_from_string},
    store, sync_events, types, verification, vodozemac,
};

/// State machine implementation of the Olm/Megolm encryption protocol
/// used for Matrix end to end encryption.
#[wasm_bindgen]
#[derive(Debug, Clone)]
pub struct OlmMachine {
    inner: matrix_sdk_crypto::OlmMachine,
}

#[wasm_bindgen]
impl OlmMachine {
    /// Constructor will always fail. To create a new `OlmMachine`, please use
    /// the `initialize` method.
    ///
    /// Why this pattern? `initialize` returns a `Promise`. Returning a
    // `Promise` from a constructor is not idiomatic in JavaScript.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Result<OlmMachine, JsError> {
        Err(JsError::new("To build an `OlmMachine`, please use the `initialize` method"))
    }

    /// Create a new `OlmMachine`.
    ///
    /// The created machine will keep the encryption keys either in a IndexedDB
    /// based store, or in a memory store and once the objects is dropped,
    /// the keys will be lost.
    ///
    /// # Arguments
    ///
    /// * `user_id` - represents the unique ID of the user that owns this
    /// machine.
    ///
    /// * `device_id` - represents the unique ID of the device
    /// that owns this machine.
    ///
    /// * `store_name` - The name that should be used to open the IndexedDB
    ///   based database. If this isn't provided, a memory-only store will be
    ///   used. *Note* the memory-only store will lose your E2EE keys when the
    ///   `OlmMachine` gets dropped.
    ///
    /// * `store_passphrase` - The passphrase that should be used to encrypt the
    ///   IndexedDB based
    pub fn initialize(
        user_id: &identifiers::UserId,
        device_id: &identifiers::DeviceId,
        store_name: Option<String>,
        store_passphrase: Option<String>,
    ) -> Promise {
        let user_id = user_id.inner.clone();
        let device_id = device_id.inner.clone();

        future_to_promise(async move {
            let store = match (store_name, store_passphrase) {
                // We need this `#[cfg]` because `IndexeddbCryptoStore`
                // implements `CryptoStore` only on `target_arch =
                // "wasm32"`. Without that, we could have a compilation
                // error when checking the entire workspace. In
                // practise, it doesn't impact this crate because it's
                // always compiled for `wasm32`.
                #[cfg(target_arch = "wasm32")]
                (Some(store_name), Some(mut store_passphrase)) => {
                    use std::sync::Arc;

                    use zeroize::Zeroize;

                    let store = Some(
                        matrix_sdk_indexeddb::IndexeddbCryptoStore::open_with_passphrase(
                            &store_name,
                            &store_passphrase,
                        )
                        .await
                        .map(Arc::new)?,
                    );

                    store_passphrase.zeroize();

                    store
                }

                #[cfg(target_arch = "wasm32")]
                (Some(store_name), None) => {
                    use std::sync::Arc;
                    Some(
                        matrix_sdk_indexeddb::IndexeddbCryptoStore::open_with_name(&store_name)
                            .await
                            .map(Arc::new)?,
                    )
                }

                (None, Some(_)) => {
                    return Err(anyhow::Error::msg(
                        "The `store_passphrase` has been set, but it has an effect only if \
                        `store_name` is set, which is not; please provide one",
                    ))
                }

                _ => None,
            };

            Ok(OlmMachine {
                inner: match store {
                    Some(store) => {
                        matrix_sdk_crypto::OlmMachine::with_store(
                            user_id.as_ref(),
                            device_id.as_ref(),
                            store,
                        )
                        .await?
                    }
                    None => {
                        matrix_sdk_crypto::OlmMachine::new(user_id.as_ref(), device_id.as_ref())
                            .await
                    }
                },
            })
        })
    }

    /// The unique user ID that owns this `OlmMachine` instance.
    #[wasm_bindgen(getter, js_name = "userId")]
    pub fn user_id(&self) -> identifiers::UserId {
        identifiers::UserId::from(self.inner.user_id().to_owned())
    }

    /// The unique device ID that identifies this `OlmMachine`.
    #[wasm_bindgen(getter, js_name = "deviceId")]
    pub fn device_id(&self) -> identifiers::DeviceId {
        identifiers::DeviceId::from(self.inner.device_id().to_owned())
    }

    /// Get the public parts of our Olm identity keys.
    #[wasm_bindgen(getter, js_name = "identityKeys")]
    pub fn identity_keys(&self) -> vodozemac::IdentityKeys {
        self.inner.identity_keys().into()
    }

    /// Get the display name of our own device.
    #[wasm_bindgen(getter, js_name = "displayName")]
    pub fn display_name(&self) -> Promise {
        let me = self.inner.clone();

        future_to_promise(async move { Ok(me.display_name().await?) })
    }

    /// Get all the tracked users of our own device.
    ///
    /// Returns a `Set<UserId>`.
    #[wasm_bindgen(js_name = "trackedUsers")]
    pub fn tracked_users(&self) -> Set {
        let set = Set::new(&JsValue::UNDEFINED);

        for user in self.inner.tracked_users() {
            set.add(&identifiers::UserId::from(user).into());
        }

        set
    }

    /// Update the tracked users.
    ///
    /// `users` is an iterator over user IDs that should be marked for
    /// tracking.
    ///
    /// This will mark users that weren't seen before for a key query
    /// and tracking.
    ///
    /// If the user is already known to the Olm machine, it will not
    /// be considered for a key query.
    #[wasm_bindgen(js_name = "updateTrackedUsers")]
    pub fn update_tracked_users(&self, users: &Array) -> Result<Promise, JsError> {
        let users = users
            .iter()
            .map(|user| Ok(downcast::<identifiers::UserId>(&user, "UserId")?.inner.clone()))
            .collect::<Result<Vec<ruma::OwnedUserId>, JsError>>()?;

        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            me.update_tracked_users(users.iter().map(AsRef::as_ref)).await;
            Ok(JsValue::UNDEFINED)
        }))
    }

    /// Handle to-device events and one-time key counts from a sync
    /// response.
    ///
    /// This will decrypt and handle to-device events returning the
    /// decrypted versions of them.
    ///
    /// To decrypt an event from the room timeline call
    /// `decrypt_room_event`.
    #[wasm_bindgen(js_name = "receiveSyncChanges")]
    pub fn receive_sync_changes(
        &self,
        to_device_events: &str,
        changed_devices: &sync_events::DeviceLists,
        one_time_key_counts: &Map,
        unused_fallback_keys: &Set,
    ) -> Result<Promise, JsError> {
        let to_device_events = serde_json::from_str(to_device_events)?;
        let changed_devices = changed_devices.inner.clone();
        let one_time_key_counts: BTreeMap<DeviceKeyAlgorithm, UInt> = one_time_key_counts
            .entries()
            .into_iter()
            .filter_map(|js_value| {
                let pair = Array::from(&js_value.ok()?);
                let (key, value) = (
                    DeviceKeyAlgorithm::from(pair.at(0).as_string()?),
                    UInt::new(pair.at(1).as_f64()? as u64)?,
                );

                Some((key, value))
            })
            .collect();
        let unused_fallback_keys: Option<Vec<DeviceKeyAlgorithm>> = Some(
            unused_fallback_keys
                .values()
                .into_iter()
                .filter_map(|js_value| Some(DeviceKeyAlgorithm::from(js_value.ok()?.as_string()?)))
                .collect(),
        );

        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            Ok(serde_json::to_string(
                &me.receive_sync_changes(
                    to_device_events,
                    &changed_devices,
                    &one_time_key_counts,
                    unused_fallback_keys.as_deref(),
                )
                .await?,
            )?)
        }))
    }

    /// Get the outgoing requests that need to be sent out.
    ///
    /// This returns a list of `JsValue` to represent either:
    ///   * `KeysUploadRequest`,
    ///   * `KeysQueryRequest`,
    ///   * `KeysClaimRequest`,
    ///   * `ToDeviceRequest`,
    ///   * `SignatureUploadRequest`,
    ///   * `RoomMessageRequest` or
    ///   * `KeysBackupRequest`.
    ///
    /// Those requests need to be sent out to the server and the
    /// responses need to be passed back to the state machine using
    /// `mark_request_as_sent`.
    #[wasm_bindgen(js_name = "outgoingRequests")]
    pub fn outgoing_requests(&self) -> Promise {
        let me = self.inner.clone();

        future_to_promise(async move {
            Ok(me
                .outgoing_requests()
                .await?
                .into_iter()
                .map(OutgoingRequest)
                .map(TryFrom::try_from)
                .collect::<Result<Vec<JsValue>, _>>()?
                .into_iter()
                .collect::<Array>())
        })
    }

    /// Mark the request with the given request ID as sent (see
    /// `outgoing_requests`).
    ///
    /// Arguments are:
    ///
    /// * `request_id` represents the unique ID of the request that was sent
    ///   out. This is needed to couple the response with the now sent out
    ///   request.
    /// * `response_type` represents the type of the request that was sent out.
    /// * `response` represents the response that was received from the server
    ///   after the outgoing request was sent out.
    #[wasm_bindgen(js_name = "markRequestAsSent")]
    pub fn mark_request_as_sent(
        &self,
        request_id: &str,
        request_type: requests::RequestType,
        response: &str,
    ) -> Result<Promise, JsError> {
        let transaction_id = OwnedTransactionId::from(request_id);
        let response = response_from_string(response)?;
        let incoming_response = responses::OwnedResponse::try_from((request_type, response))?;

        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            Ok(me.mark_request_as_sent(&transaction_id, &incoming_response).await.map(|_| true)?)
        }))
    }

    /// Encrypt a room message for the given room.
    ///
    /// Beware that a room key needs to be shared before this
    /// method can be called using the `share_room_key` method.
    ///
    /// `room_id` is the ID of the room for which the message should
    /// be encrypted. `event_type` is the type of the event. `content`
    /// is the plaintext content of the message that should be
    /// encrypted.
    ///
    /// # Panics
    ///
    /// Panics if a group session for the given room wasn't shared
    /// beforehand.
    #[wasm_bindgen(js_name = "encryptRoomEvent")]
    pub fn encrypt_room_event(
        &self,
        room_id: &identifiers::RoomId,
        event_type: String,
        content: &str,
    ) -> Result<Promise, JsError> {
        let room_id = room_id.inner.clone();
        let content: JsonValue = serde_json::from_str(content)?;
        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            Ok(serde_json::to_string(
                &me.encrypt_room_event_raw(&room_id, content, event_type.as_ref()).await?,
            )?)
        }))
    }

    /// Decrypt an event from a room timeline.
    ///
    /// # Arguments
    ///
    /// * `event`, the event that should be decrypted.
    /// * `room_id`, the ID of the room where the event was sent to.
    #[wasm_bindgen(js_name = "decryptRoomEvent")]
    pub fn decrypt_room_event(
        &self,
        event: &str,
        room_id: &identifiers::RoomId,
    ) -> Result<Promise, JsError> {
        let event: Raw<_> = serde_json::from_str(event)?;
        let room_id = room_id.inner.clone();
        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            let room_event = me.decrypt_room_event(&event, room_id.as_ref()).await?;

            Ok(responses::DecryptedRoomEvent::from(room_event))
        }))
    }

    /// Get the status of the private cross signing keys.
    ///
    /// This can be used to check which private cross signing keys we
    /// have stored locally.
    #[wasm_bindgen(js_name = "crossSigningStatus")]
    pub fn cross_signing_status(&self) -> Promise {
        let me = self.inner.clone();

        future_to_promise::<_, olm::CrossSigningStatus>(async move {
            Ok(me.cross_signing_status().await.into())
        })
    }

    /// Export all the private cross signing keys we have.
    ///
    /// The export will contain the seed for the ed25519 keys as a
    /// unpadded base64 encoded string.
    ///
    /// This method returns None if we don’t have any private cross
    /// signing keys.
    #[wasm_bindgen(js_name = "exportCrossSigningKeys")]
    pub fn export_cross_signing_keys(&self) -> Promise {
        let me = self.inner.clone();

        future_to_promise(async move {
            Ok(me.export_cross_signing_keys().await.map(store::CrossSigningKeyExport::from))
        })
    }

    /// Import our private cross signing keys.
    ///
    /// The export needs to contain the seed for the ed25519 keys as
    /// an unpadded base64 encoded string.
    #[wasm_bindgen(js_name = "importCrossSigningKeys")]
    pub fn import_cross_signing_keys(&self, export: store::CrossSigningKeyExport) -> Promise {
        let me = self.inner.clone();
        let export = export.inner;

        future_to_promise(async move {
            Ok(me.import_cross_signing_keys(export).await.map(olm::CrossSigningStatus::from)?)
        })
    }

    /// Create a new cross signing identity and get the upload request
    /// to push the new public keys to the server.
    ///
    /// Warning: This will delete any existing cross signing keys that
    /// might exist on the server and thus will reset the trust
    /// between all the devices.
    ///
    /// Uploading these keys will require user interactive auth.
    #[wasm_bindgen(js_name = "bootstrapCrossSigning")]
    pub fn bootstrap_cross_signing(&self, reset: bool) -> Promise {
        let me = self.inner.clone();

        future_to_promise(async move {
            let (upload_signing_keys_request, upload_signatures_request) =
                me.bootstrap_cross_signing(reset).await?;

            let tuple = Array::new();
            tuple.set(
                0,
                requests::SigningKeysUploadRequest::try_from(&upload_signing_keys_request)?.into(),
            );
            tuple.set(
                1,
                requests::SignatureUploadRequest::try_from(&upload_signatures_request)?.into(),
            );

            Ok(tuple)
        })
    }

    /// Get the cross signing user identity of a user.
    #[wasm_bindgen(js_name = "getIdentity")]
    pub fn get_identity(&self, user_id: &identifiers::UserId) -> Promise {
        let me = self.inner.clone();
        let user_id = user_id.inner.clone();

        future_to_promise(async move {
            Ok(me.get_identity(user_id.as_ref(), None).await?.map(identities::UserIdentities::from))
        })
    }

    /// Sign the given message using our device key and if available
    /// cross-signing master key.
    pub fn sign(&self, message: String) -> Promise {
        let me = self.inner.clone();

        future_to_promise::<_, types::Signatures>(async move { Ok(me.sign(&message).await.into()) })
    }

    /// Invalidate the currently active outbound group session for the
    /// given room.
    ///
    /// Returns true if a session was invalidated, false if there was
    /// no session to invalidate.
    #[wasm_bindgen(js_name = "invalidateGroupSession")]
    pub fn invalidate_group_session(&self, room_id: &identifiers::RoomId) -> Promise {
        let room_id = room_id.inner.clone();
        let me = self.inner.clone();

        future_to_promise(async move { Ok(me.invalidate_group_session(&room_id).await?) })
    }

    /// Get to-device requests to share a room key with users in a room.
    ///
    /// `room_id` is the room ID. `users` is an array of `UserId`
    /// objects. `encryption_settings` are an `EncryptionSettings`
    /// object.
    #[wasm_bindgen(js_name = "shareRoomKey")]
    pub fn share_room_key(
        &self,
        room_id: &identifiers::RoomId,
        users: &Array,
        encryption_settings: &encryption::EncryptionSettings,
    ) -> Result<Promise, JsError> {
        let room_id = room_id.inner.clone();
        let users = users
            .iter()
            .map(|user| Ok(downcast::<identifiers::UserId>(&user, "UserId")?.inner.clone()))
            .collect::<Result<Vec<ruma::OwnedUserId>, JsError>>()?;
        let encryption_settings =
            matrix_sdk_crypto::olm::EncryptionSettings::from(encryption_settings);

        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            Ok(serde_json::to_string(
                &me.share_room_key(&room_id, users.iter().map(AsRef::as_ref), encryption_settings)
                    .await?,
            )?)
        }))
    }

    /// Get the a key claiming request for the user/device pairs that
    /// we are missing Olm sessions for.
    ///
    /// Returns `NULL` if no key claiming request needs to be sent
    /// out, otherwise it returns an `Array` where the first key is
    /// the transaction ID as a string, and the second key is the keys
    /// claim request serialized to JSON.
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
    /// `users` represents the list of users that we should check if
    /// we lack a session with one of their devices. This can be an
    /// empty iterator when calling this method between sync requests.
    #[wasm_bindgen(js_name = "getMissingSessions")]
    pub fn get_missing_sessions(&self, users: &Array) -> Result<Promise, JsError> {
        let users = users
            .iter()
            .map(|user| Ok(downcast::<identifiers::UserId>(&user, "UserId")?.inner.clone()))
            .collect::<Result<Vec<ruma::OwnedUserId>, JsError>>()?;

        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            match me.get_missing_sessions(users.iter().map(AsRef::as_ref)).await? {
                Some((transaction_id, keys_claim_request)) => {
                    Ok(JsValue::from(requests::KeysClaimRequest::try_from((
                        transaction_id.to_string(),
                        &keys_claim_request,
                    ))?))
                }

                None => Ok(JsValue::NULL),
            }
        }))
    }

    /// Get a map holding all the devices of a user.
    ///
    /// `user_id` represents the unique ID of the user that the
    /// devices belong to.
    #[wasm_bindgen(js_name = "getUserDevices")]
    pub fn get_user_devices(&self, user_id: &identifiers::UserId) -> Promise {
        let user_id = user_id.inner.clone();

        let me = self.inner.clone();

        future_to_promise::<_, device::UserDevices>(async move {
            Ok(me.get_user_devices(&user_id, None).await.map(Into::into)?)
        })
    }

    /// Get a specific device of a user if one is found and the crypto store
    /// didn't throw an error.
    ///
    /// `user_id` represents the unique ID of the user that the
    /// identity belongs to. `device_id` represents the unique ID of
    /// the device.
    #[wasm_bindgen(js_name = "getDevice")]
    pub fn get_device(
        &self,
        user_id: &identifiers::UserId,
        device_id: &identifiers::DeviceId,
    ) -> Promise {
        let user_id = user_id.inner.clone();
        let device_id = device_id.inner.clone();

        let me = self.inner.clone();

        future_to_promise::<_, Option<device::Device>>(async move {
            Ok(me.get_device(&user_id, &device_id, None).await?.map(Into::into))
        })
    }

    /// Get a verification object for the given user ID with the given
    /// flow ID (a to-device request ID if the verification has been
    /// requested by a to-device request, or a room event ID if the
    /// verification has been requested by a room event).
    ///
    /// It returns a “`Verification` object”, which is either a `Sas`
    /// or `Qr` object.
    #[wasm_bindgen(js_name = "getVerification")]
    pub fn get_verification(
        &self,
        user_id: &identifiers::UserId,
        flow_id: &str,
    ) -> Result<JsValue, JsError> {
        self.inner
            .get_verification(&user_id.inner, flow_id)
            .map(verification::Verification)
            .map(JsValue::try_from)
            .transpose()
            .map(JsValue::from)
    }

    /// Get a verification request object with the given flow ID.
    #[wasm_bindgen(js_name = "getVerificationRequest")]
    pub fn get_verification_request(
        &self,
        user_id: &identifiers::UserId,
        flow_id: &str,
    ) -> Option<verification::VerificationRequest> {
        self.inner.get_verification_request(&user_id.inner, flow_id).map(Into::into)
    }

    /// Get all the verification requests of a given user.
    #[wasm_bindgen(js_name = "getVerificationRequests")]
    pub fn get_verification_requests(&self, user_id: &identifiers::UserId) -> Array {
        self.inner
            .get_verification_requests(&user_id.inner)
            .into_iter()
            .map(verification::VerificationRequest::from)
            .map(JsValue::from)
            .collect()
    }

    /// Receive an unencrypted verification event.
    ///
    /// This method can be used to pass verification events that are
    /// happening in unencrypted rooms to the `OlmMachine`.
    ///
    /// Note: This does not need to be called for encrypted events
    /// since those will get passed to the `OlmMachine` during
    /// decryption.
    #[wasm_bindgen(js_name = "receiveUnencryptedVerificationEvent")]
    pub fn receive_unencrypted_verification_event(&self, event: &str) -> Result<Promise, JsError> {
        let event: ruma::events::AnyMessageLikeEvent = serde_json::from_str(event)?;
        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            Ok(me
                .receive_unencrypted_verification_event(&event)
                .await
                .map(|_| JsValue::UNDEFINED)?)
        }))
    }

    /// Export the keys that match the given predicate.
    ///
    /// `predicate` is a closure that will be called for every known
    /// `InboundGroupSession`, which represents a room key. If the closure
    /// returns `true`, the `InboundGroupSession` will be included in the
    /// export, otherwise it won't.
    #[wasm_bindgen(js_name = "exportRoomKeys")]
    pub fn export_room_keys(&self, predicate: Function) -> Promise {
        let me = self.inner.clone();

        future_to_promise(async move {
            Ok(serde_json::to_string(
                &me.export_room_keys(|session| {
                    let session = session.clone();

                    predicate
                        .call1(&JsValue::NULL, &olm::InboundGroupSession::from(session).into())
                        .expect("Predicate function passed to `export_room_keys` failed")
                        .as_bool()
                        .unwrap_or(false)
                })
                .await?,
            )?)
        })
    }

    /// Import the given room keys into our store.
    ///
    /// `exported_keys` is a list of previously exported keys that should be
    /// imported into our store. If we already have a better version of a key,
    /// the key will _not_ be imported.
    ///
    /// `progress_listener` is a closure that takes 2 arguments: `progress` and
    /// `total`, and returns nothing.
    #[wasm_bindgen(js_name = "importRoomKeys")]
    pub fn import_room_keys(
        &self,
        exported_room_keys: &str,
        progress_listener: Function,
    ) -> Result<Promise, JsError> {
        let me = self.inner.clone();
        let exported_room_keys: Vec<matrix_sdk_crypto::olm::ExportedRoomKey> =
            serde_json::from_str(exported_room_keys)?;

        Ok(future_to_promise(async move {
            let matrix_sdk_crypto::RoomKeyImportResult { imported_count, total_count, keys } = me
                .import_room_keys(exported_room_keys, false, |progress, total| {
                    let progress: u64 = progress.try_into().unwrap();
                    let total: u64 = total.try_into().unwrap();

                    progress_listener
                        .call2(&JsValue::NULL, &JsValue::from(progress), &JsValue::from(total))
                        .expect("Progress listener passed to `import_room_keys` failed");
                })
                .await?;

            Ok(serde_json::to_string(&json!({
                "imported_count": imported_count,
                "total_count": total_count,
                "keys": keys,
            }))?)
        }))
    }

    /// Encrypt the list of exported room keys using the given passphrase.
    ///
    /// `exported_room_keys` is a list of sessions that should be encrypted
    /// (it's generally returned by `export_room_keys`). `passphrase` is the
    /// passphrase that will be used to encrypt the exported room keys. And
    /// `rounds` is the number of rounds that should be used for the key
    /// derivation when the passphrase gets turned into an AES key. More rounds
    /// are increasingly computationnally intensive and as such help against
    /// brute-force attacks. Should be at least `10_000`, while values in the
    /// `100_000` ranges should be preferred.
    #[wasm_bindgen(js_name = "encryptExportedRoomKeys")]
    pub fn encrypt_exported_room_keys(
        exported_room_keys: &str,
        passphrase: &str,
        rounds: u32,
    ) -> Result<String, JsError> {
        let exported_room_keys: Vec<matrix_sdk_crypto::olm::ExportedRoomKey> =
            serde_json::from_str(exported_room_keys)?;

        Ok(matrix_sdk_crypto::encrypt_room_key_export(&exported_room_keys, passphrase, rounds)?)
    }

    /// Try to decrypt a reader into a list of exported room keys.
    ///
    /// `encrypted_exported_room_keys` is the result from
    /// `encrypt_exported_room_keys`. `passphrase` is the passphrase that was
    /// used when calling `encrypt_exported_room_keys`.
    #[wasm_bindgen(js_name = "decryptExportedRoomKeys")]
    pub fn decrypt_exported_room_keys(
        encrypted_exported_room_keys: &str,
        passphrase: &str,
    ) -> Result<String, JsError> {
        Ok(serde_json::to_string(&matrix_sdk_crypto::decrypt_room_key_export(
            encrypted_exported_room_keys.as_bytes(),
            passphrase,
        )?)?)
    }

    /// Shut down the `OlmMachine`.
    ///
    /// The `OlmMachine` cannot be used after this method has been called.
    ///
    /// All associated resources will be closed too, like IndexedDB
    /// connections.
    pub fn close(self) {}
}
