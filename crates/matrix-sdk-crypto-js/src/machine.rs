use std::{collections::BTreeMap, sync::Arc, time::Duration};

use js_sys::{Array, Map, Promise, Set};
use ruma::{
    events::{AnyMessageLikeEventContent, EventContent},
    DeviceKeyAlgorithm, OwnedTransactionId, UInt,
};
use serde_json::value::RawValue as RawJsonValue;
use wasm_bindgen::prelude::*;

use crate::{
    downcast, events,
    future::future_to_promise,
    identifiers, requests,
    requests::OutgoingRequest,
    responses::{self, response_from_string},
    sync_events,
};

#[wasm_bindgen]
#[derive(Debug)]
pub struct OlmMachine {
    inner: Arc<matrix_sdk_crypto::OlmMachine>,
}

#[cfg_attr(feature = "js", wasm_bindgen)]
impl OlmMachine {
    #[wasm_bindgen(constructor)]
    pub fn new(user_id: &identifiers::UserId, device_id: &identifiers::DeviceId) -> Promise {
        let user_id = user_id.inner.clone();
        let device_id = device_id.inner.clone();

        future_to_promise(async move {
            Ok(OlmMachine {
                inner: Arc::new(
                    matrix_sdk_crypto::OlmMachine::new(user_id.as_ref(), device_id.as_ref()).await,
                ),
            })
        })
    }

    /// The unique user ID that owns this `OlmMachine` instance.
    #[wasm_bindgen(js_name = "userId")]
    pub fn user_id(&self) -> identifiers::UserId {
        identifiers::UserId { inner: self.inner.user_id().to_owned() }
    }

    /// The unique device ID that identifies this `OlmMachine`.
    #[wasm_bindgen(js_name = "deviceId")]
    pub fn device_id(&self) -> identifiers::DeviceId {
        identifiers::DeviceId { inner: self.inner.device_id().to_owned() }
    }

    ///// Get the public parts of our Olm identity keys.
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

        self.inner
            .tracked_users()
            .into_iter()
            .map(|user| identifiers::UserId { inner: user })
            .for_each(|user| {
                set.add(&user.into());
            });

        set
    }

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
    /// `request_id` represents the unique ID of the request that was
    /// sent out. This is needed to couple the response with the now
    /// sent out request. `response_type` represents the type of the
    /// request that was sent out. `response` represents the response
    /// that was received from the server after the outgoing request
    /// was sent out. `
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
    /// Since group sessions can expire or become invalid if the room
    /// membership changes, client authors should check with the
    /// `should_share_group_session` method if a new group session
    /// needs to be shared.
    ///
    /// `room_id` is the ID of the room for which the message should
    /// be encrypted. `event_type` is the type of the event. `content`
    /// is the plaintext content of the message that should be
    /// encrypted.
    ///
    /// # Panics
    ///
    /// Panics if a group session for the given room wasn't shared beforehand.
    pub fn encrypt(
        &self,
        room_id: &identifiers::RoomId,
        event_type: &str,
        content: &str,
    ) -> Result<Promise, JsError> {
        let room_id = room_id.inner.clone();
        let content: Box<RawJsonValue> = serde_json::from_str(content)?;
        let content = AnyMessageLikeEventContent::from_parts(event_type, &content)?;

        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            Ok(serde_json::to_string(&me.encrypt(&room_id, content).await?)?)
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

#[wasm_bindgen]
#[derive(Debug, Clone)]
pub struct Ed25519PublicKey {
    inner: vodozemac::Ed25519PublicKey,
}

#[wasm_bindgen]
impl Ed25519PublicKey {
    #[wasm_bindgen(getter)]
    pub fn length(&self) -> usize {
        vodozemac::Ed25519PublicKey::LENGTH
    }

    #[wasm_bindgen(js_name = "toBase64")]
    pub fn to_base64(&self) -> String {
        self.inner.to_base64()
    }
}

#[wasm_bindgen]
#[derive(Debug, Clone)]
pub struct Curve25519PublicKey {
    inner: vodozemac::Curve25519PublicKey,
}

#[wasm_bindgen]
impl Curve25519PublicKey {
    #[wasm_bindgen(getter)]
    pub fn length(&self) -> usize {
        vodozemac::Curve25519PublicKey::LENGTH
    }

    #[wasm_bindgen(js_name = "toBase64")]
    pub fn to_base64(&self) -> String {
        self.inner.to_base64()
    }
}

#[wasm_bindgen(getter_with_clone)]
#[derive(Debug)]
pub struct IdentityKeys {
    pub ed25519: Ed25519PublicKey,
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

impl Into<EncryptionAlgorithm> for ruma::EventEncryptionAlgorithm {
    fn into(self) -> EncryptionAlgorithm {
        use EncryptionAlgorithm::*;

        match self {
            Self::OlmV1Curve25519AesSha2 => OlmV1Curve25519AesSha2,
            Self::MegolmV1AesSha2 => MegolmV1AesSha2,
            _ => unreachable!("Unknown variant"),
        }
    }
}

#[wasm_bindgen(getter_with_clone)]
#[derive(Debug, Clone)]
pub struct EncryptionSettings {
    /// The algorith, see `EncryptionAlgorithm`.
    pub algorithm: EncryptionAlgorithm,

    /// A duration expressed in microseconds.
    #[wasm_bindgen(js_name = "rotationPeriod")]
    pub rotation_period: u64,

    #[wasm_bindgen(js_name = "rotationPeriodMessages")]
    pub rotation_period_messages: u64,

    #[wasm_bindgen(js_name = "historyVisibility")]
    pub history_visibility: events::HistoryVisibility,
}

#[wasm_bindgen]
impl EncryptionSettings {
    /// Create a new `EncryptionSettings` with default values.
    #[wasm_bindgen(constructor)]
    pub fn new() -> EncryptionSettings {
        let default = matrix_sdk_crypto::olm::EncryptionSettings::default();

        Self {
            algorithm: default.algorithm.into(),
            rotation_period: default.rotation_period.as_micros().try_into().unwrap(),
            rotation_period_messages: default.rotation_period_msgs,
            history_visibility: default.history_visibility.into(),
        }
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
