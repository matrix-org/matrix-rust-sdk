//! The crypto specific Olm objects.

use std::{collections::BTreeMap, time::Duration};

use js_sys::{Array, Map, Promise, Set};
use ruma::{DeviceKeyAlgorithm, OwnedTransactionId, UInt};
use serde_json::Value as JsonValue;
use wasm_bindgen::prelude::*;

use crate::{
    downcast, events,
    future::future_to_promise,
    identifiers, requests,
    requests::OutgoingRequest,
    responses::{self, response_from_string},
    sync_events,
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
    /// Create a new memory based `OlmMachine`.
    ///
    /// The created machine will keep the encryption keys only in
    /// memory and once the objects is dropped, the keys will be lost.
    ///
    /// `user_id` represents the unique ID of the user that owns this
    /// machine. `device_id` represents the unique ID of the device
    /// that owns this machine.
    #[wasm_bindgen(constructor)]
    #[allow(clippy::new_ret_no_self)]
    pub fn new(user_id: &identifiers::UserId, device_id: &identifiers::DeviceId) -> Promise {
        let user_id = user_id.inner.clone();
        let device_id = device_id.inner.clone();

        future_to_promise(async move {
            Ok(OlmMachine {
                inner: matrix_sdk_crypto::OlmMachine::new(user_id.as_ref(), device_id.as_ref())
                    .await,
            })
        })
    }

    /// The unique user ID that owns this `OlmMachine` instance.
    #[wasm_bindgen(js_name = "userId")]
    pub fn user_id(&self) -> identifiers::UserId {
        identifiers::UserId::new_with(self.inner.user_id().to_owned())
    }

    /// The unique device ID that identifies this `OlmMachine`.
    #[wasm_bindgen(js_name = "deviceId")]
    pub fn device_id(&self) -> identifiers::DeviceId {
        identifiers::DeviceId::new_with(self.inner.device_id().to_owned())
    }

    /// Get the public parts of our Olm identity keys.
    #[wasm_bindgen(js_name = "identityKeys")]
    pub fn identity_keys(&self) -> IdentityKeys {
        self.inner.identity_keys().into()
    }

    /// Get the display name of our own device.
    #[wasm_bindgen(js_name = "displayName")]
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

        self.inner.tracked_users().into_iter().map(identifiers::UserId::new_with).for_each(
            |user| {
                set.add(&user.into());
            },
        );

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
    /// Beware that a group session needs to be shared before this
    /// method can be called using the `share_group_session` method.
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

    /// Get to-device requests to share a group session with users in a room.
    ///
    /// `room_id` is the room ID. `users` is an array of `UserId`
    /// objects. `encryption_settings` are an `EncryptionSettings`
    /// object.
    #[wasm_bindgen(js_name = "shareGroupSession")]
    pub fn share_group_session(
        &self,
        room_id: &identifiers::RoomId,
        users: &Array,
        encryption_settings: &EncryptionSettings,
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
                &me.share_group_session(
                    &room_id,
                    users.iter().map(AsRef::as_ref),
                    encryption_settings,
                )
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
}

/// An Ed25519 public key, used to verify digital signatures.
#[wasm_bindgen]
#[derive(Debug, Clone)]
pub struct Ed25519PublicKey {
    inner: vodozemac::Ed25519PublicKey,
}

#[wasm_bindgen]
impl Ed25519PublicKey {
    /// The number of bytes an Ed25519 public key has.
    #[wasm_bindgen(getter)]
    pub fn length(&self) -> usize {
        vodozemac::Ed25519PublicKey::LENGTH
    }

    /// Serialize an Ed25519 public key to an unpadded base64
    /// representation.
    #[wasm_bindgen(js_name = "toBase64")]
    pub fn to_base64(&self) -> String {
        self.inner.to_base64()
    }
}

/// A Curve25519 public key.
#[wasm_bindgen]
#[derive(Debug, Clone)]
pub struct Curve25519PublicKey {
    inner: vodozemac::Curve25519PublicKey,
}

#[wasm_bindgen]
impl Curve25519PublicKey {
    /// The number of bytes a Curve25519 public key has.
    #[wasm_bindgen(getter)]
    pub fn length(&self) -> usize {
        vodozemac::Curve25519PublicKey::LENGTH
    }

    /// Serialize an Curve25519 public key to an unpadded base64
    /// representation.
    #[wasm_bindgen(js_name = "toBase64")]
    pub fn to_base64(&self) -> String {
        self.inner.to_base64()
    }
}

/// Struct holding the two public identity keys of an account.
#[wasm_bindgen(getter_with_clone)]
#[derive(Debug)]
pub struct IdentityKeys {
    /// The Ed25519 public key, used for signing.
    pub ed25519: Ed25519PublicKey,

    /// The Curve25519 public key, used for establish shared secrets.
    pub curve25519: Curve25519PublicKey,
}

impl From<matrix_sdk_crypto::olm::IdentityKeys> for IdentityKeys {
    fn from(value: matrix_sdk_crypto::olm::IdentityKeys) -> Self {
        Self {
            ed25519: Ed25519PublicKey { inner: value.ed25519 },
            curve25519: Curve25519PublicKey { inner: value.curve25519 },
        }
    }
}

/// An encryption algorithm to be used to encrypt messages sent to a
/// room.
#[wasm_bindgen]
#[derive(Debug, Clone)]
pub enum EncryptionAlgorithm {
    /// Olm version 1 using Curve25519, AES-256, and SHA-256.
    OlmV1Curve25519AesSha2,

    /// Megolm version 1 using AES-256 and SHA-256.
    MegolmV1AesSha2,
}

impl From<EncryptionAlgorithm> for ruma::EventEncryptionAlgorithm {
    fn from(value: EncryptionAlgorithm) -> Self {
        use EncryptionAlgorithm::*;

        match value {
            OlmV1Curve25519AesSha2 => Self::OlmV1Curve25519AesSha2,
            MegolmV1AesSha2 => Self::MegolmV1AesSha2,
        }
    }
}

impl From<ruma::EventEncryptionAlgorithm> for EncryptionAlgorithm {
    fn from(value: ruma::EventEncryptionAlgorithm) -> Self {
        use ruma::EventEncryptionAlgorithm::*;

        match value {
            OlmV1Curve25519AesSha2 => Self::OlmV1Curve25519AesSha2,
            MegolmV1AesSha2 => Self::MegolmV1AesSha2,
            _ => unreachable!("Unknown variant"),
        }
    }
}

/// Settings for an encrypted room.
///
/// This determines the algorithm and rotation periods of a group
/// session.
#[wasm_bindgen(getter_with_clone)]
#[derive(Debug, Clone)]
pub struct EncryptionSettings {
    /// The encryption algorithm that should be used in the room.
    pub algorithm: EncryptionAlgorithm,

    /// How long the session should be used before changing it,
    /// expressed in microseconds.
    #[wasm_bindgen(js_name = "rotationPeriod")]
    pub rotation_period: u64,

    /// How many messages should be sent before changing the session.
    #[wasm_bindgen(js_name = "rotationPeriodMessages")]
    pub rotation_period_messages: u64,

    /// The history visibility of the room when the session was
    /// created.
    #[wasm_bindgen(js_name = "historyVisibility")]
    pub history_visibility: events::HistoryVisibility,
}

impl Default for EncryptionSettings {
    fn default() -> Self {
        let default = matrix_sdk_crypto::olm::EncryptionSettings::default();

        Self {
            algorithm: default.algorithm.into(),
            rotation_period: default.rotation_period.as_micros().try_into().unwrap(),
            rotation_period_messages: default.rotation_period_msgs,
            history_visibility: default.history_visibility.into(),
        }
    }
}

#[wasm_bindgen]
impl EncryptionSettings {
    /// Create a new `EncryptionSettings` with default values.
    #[wasm_bindgen(constructor)]
    pub fn new() -> EncryptionSettings {
        Self::default()
    }
}

impl From<&EncryptionSettings> for matrix_sdk_crypto::olm::EncryptionSettings {
    fn from(value: &EncryptionSettings) -> Self {
        Self {
            algorithm: value.algorithm.clone().into(),
            rotation_period: Duration::from_micros(value.rotation_period),
            rotation_period_msgs: value.rotation_period_messages,
            history_visibility: value.history_visibility.clone().into(),
        }
    }
}
