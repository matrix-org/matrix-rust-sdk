//! The crypto specific Olm objects.

use std::{
    collections::{BTreeMap, HashMap},
    time::Duration,
};

use napi::bindgen_prelude::{Either3, Either5, ToNapiValue};
use napi_derive::*;
use ruma::{DeviceKeyAlgorithm, OwnedTransactionId, UInt};
use serde_json::Value as JsonValue;

use crate::{
    events, identifiers, into_err, requests, responses, responses::response_from_string,
    sync_events,
};

/// State machine implementation of the Olm/Megolm encryption protocol
/// used for Matrix end to end encryption.
#[napi]
pub struct OlmMachine {
    inner: matrix_sdk_crypto::OlmMachine,
}

#[napi]
impl OlmMachine {
    // napi doesn't support `#[napi(factory)]` with an `async fn`. So
    // we create a normal `async fn` function, and then, we create a
    // constructor that raises an error.

    #[napi]
    pub async fn initialize(
        user_id: &identifiers::UserId,
        device_id: &identifiers::DeviceId,
    ) -> Self {
        let user_id = user_id.inner.clone();
        let device_id = device_id.inner.clone();

        OlmMachine {
            inner: matrix_sdk_crypto::OlmMachine::new(user_id.as_ref(), device_id.as_ref()).await,
        }
    }

    #[napi(constructor)]
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> napi::Error {
        napi::Error::from_reason("To build an `OldMachine`, please use the `initialize` method")
    }

    #[napi]
    pub fn user_id(&self) -> identifiers::UserId {
        identifiers::UserId::new_with(self.inner.user_id().to_owned())
    }

    #[napi]
    pub fn device_id(&self) -> identifiers::DeviceId {
        identifiers::DeviceId::new_with(self.inner.device_id().to_owned())
    }

    #[napi]
    pub fn identity_keys(&self) -> IdentityKeys {
        self.inner.identity_keys().into()
    }

    #[napi]
    pub async fn receive_sync_changes(
        &self,
        to_device_events: String,
        changed_devices: &sync_events::DeviceLists,
        one_time_key_counts: HashMap<String, u32>,
        unused_fallback_keys: Vec<String>,
    ) -> Result<String, napi::Error> {
        let to_device_events = serde_json::from_str(to_device_events.as_ref()).map_err(into_err)?;
        let changed_devices = changed_devices.inner.clone();
        let one_time_key_counts = one_time_key_counts
            .iter()
            .filter_map(|(key, value)| {
                Some((DeviceKeyAlgorithm::from(key.as_str()), UInt::new(*value as u64)?))
            })
            .collect::<BTreeMap<DeviceKeyAlgorithm, UInt>>();
        let unused_fallback_keys = Some(
            unused_fallback_keys
                .into_iter()
                .map(|key| DeviceKeyAlgorithm::from(key.as_str()))
                .collect::<Vec<DeviceKeyAlgorithm>>(),
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

    // We could be tempted to use `requests::OutgoingRequests` as its
    // a type alias for this giant `Either` chain. But `napi` won't
    // unfold it properly into a valid TypeScript definition, so…
    // let's copy-paste :-(.
    #[napi]
    pub async fn outgoing_requests(
        &self,
    ) -> Result<
        Vec<
            Either5<
                requests::KeysUploadRequest,
                requests::KeysQueryRequest,
                requests::KeysClaimRequest,
                requests::ToDeviceRequest,
                Either3<
                    requests::SignatureUploadRequest,
                    requests::RoomMessageRequest,
                    requests::KeysBackupRequest,
                >,
            >,
        >,
        napi::Error,
    > {
        self.inner
            .outgoing_requests()
            .await
            .map_err(into_err)?
            .into_iter()
            .map(requests::OutgoingRequest)
            .map(TryFrom::try_from)
            .collect::<Result<Vec<_>, _>>()
            .map_err(into_err)
    }

    #[napi]
    pub async fn mark_request_as_sent(
        &self,
        request_id: String,
        request_type: requests::RequestType,
        response: String,
    ) -> Result<bool, napi::Error> {
        let transaction_id = OwnedTransactionId::from(request_id);
        let response = response_from_string(response.as_str()).map_err(into_err)?;
        let incoming_response = responses::OwnedResponse::try_from((request_type, response))?;

        self.inner
            .mark_request_as_sent(&transaction_id, &incoming_response)
            .await
            .map(|_| true)
            .map_err(into_err)
    }

    #[napi]
    pub async fn get_missing_sessions(
        &self,
        users: Vec<&identifiers::UserId>,
    ) -> Result<Option<requests::KeysClaimRequest>, napi::Error> {
        let users =
            users.into_iter().map(|user| user.inner.clone()).collect::<Vec<ruma::OwnedUserId>>();

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

    #[napi]
    pub async fn update_tracked_users(&self, users: Vec<&identifiers::UserId>) {
        let users =
            users.into_iter().map(|user| user.inner.clone()).collect::<Vec<ruma::OwnedUserId>>();

        self.inner.update_tracked_users(users.iter().map(AsRef::as_ref)).await;
    }

    #[napi]
    pub async fn share_room_key(
        &self,
        room_id: &identifiers::RoomId,
        users: Vec<&identifiers::UserId>,
        encryption_settings: &EncryptionSettings,
    ) -> Result<String, napi::Error> {
        let room_id = room_id.inner.clone();
        let users =
            users.into_iter().map(|user| user.inner.clone()).collect::<Vec<ruma::OwnedUserId>>();
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

    #[napi]
    pub async fn encrypt_room_event(
        &self,
        room_id: &identifiers::RoomId,
        event_type: String,
        content: String,
    ) -> Result<String, napi::Error> {
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
}

/// An Ed25519 public key, used to verify digital signatures.
#[napi]
#[derive(Clone)]
pub struct Ed25519PublicKey {
    inner: vodozemac::Ed25519PublicKey,
}

#[napi]
impl Ed25519PublicKey {
    /// The number of bytes an Ed25519 public key has.
    #[napi(getter)]
    pub fn length(&self) -> u32 {
        vodozemac::Ed25519PublicKey::LENGTH as u32
    }

    /// Serialize an Ed25519 public key to an unpadded base64
    /// representation.
    #[napi(js_name = "toBase64")]
    pub fn to_base64(&self) -> String {
        self.inner.to_base64()
    }
}

/// A Curve25519 public key.
#[napi]
#[derive(Clone)]
pub struct Curve25519PublicKey {
    inner: vodozemac::Curve25519PublicKey,
}

#[napi]
impl Curve25519PublicKey {
    /// The number of bytes a Curve25519 public key has.
    #[napi(getter)]
    pub fn length(&self) -> u32 {
        vodozemac::Curve25519PublicKey::LENGTH as u32
    }

    /// Serialize an Curve25519 public key to an unpadded base64
    /// representation.
    #[napi(js_name = "toBase64")]
    pub fn to_base64(&self) -> String {
        self.inner.to_base64()
    }
}

#[napi]
pub struct IdentityKeys {
    /// The Ed25519 public key, used for signing.
    ed25519: Ed25519PublicKey,

    /// The Curve25519 public key, used for establish shared secrets.
    curve25519: Curve25519PublicKey,
}

#[napi]
impl IdentityKeys {
    #[napi(getter)]
    pub fn ed25519(&self) -> Ed25519PublicKey {
        self.ed25519.clone()
    }

    #[napi(getter)]
    pub fn curve25519(&self) -> Curve25519PublicKey {
        self.curve25519.clone()
    }
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
#[napi]
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
#[napi]
pub struct EncryptionSettings {
    /// The encryption algorithm that should be used in the room.
    pub algorithm: EncryptionAlgorithm,

    /// How long the session should be used before changing it,
    /// expressed in microseconds.
    pub rotation_period: u32,

    /// How many messages should be sent before changing the session.
    pub rotation_period_messages: u32,

    /// The history visibility of the room when the session was
    /// created.
    pub history_visibility: events::HistoryVisibility,
}

impl Default for EncryptionSettings {
    fn default() -> Self {
        let default = matrix_sdk_crypto::olm::EncryptionSettings::default();

        Self {
            algorithm: default.algorithm.into(),
            rotation_period: default.rotation_period.as_micros().try_into().unwrap(),
            rotation_period_messages: default.rotation_period_msgs.try_into().unwrap(),
            history_visibility: default.history_visibility.into(),
        }
    }
}

#[napi]
impl EncryptionSettings {
    /// Create a new `EncryptionSettings` with default values.
    #[napi(constructor)]
    pub fn new() -> EncryptionSettings {
        Self::default()
    }
}

impl From<&EncryptionSettings> for matrix_sdk_crypto::olm::EncryptionSettings {
    fn from(value: &EncryptionSettings) -> Self {
        Self {
            algorithm: value.algorithm.clone().into(),
            rotation_period: Duration::from_micros(value.rotation_period.into()),
            rotation_period_msgs: value.rotation_period_messages.into(),
            history_visibility: value.history_visibility.clone().into(),
        }
    }
}
