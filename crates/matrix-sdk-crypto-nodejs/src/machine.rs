//! The crypto specific Olm objects.

use std::collections::{BTreeMap, HashMap};

use napi_derive::*;
use ruma::{DeviceKeyAlgorithm, UInt};

use crate::{identifiers, into_err, sync_events};

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

        Ok(serde_json::to_string(
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
        .map_err(into_err)?)
    }

    #[napi]
    pub async fn update_tracked_users(&self, users: Vec<&identifiers::UserId>) {
        let users: Vec<ruma::OwnedUserId> =
            users.into_iter().map(|user| user.inner.clone()).collect();

        self.inner.update_tracked_users(users.iter().map(AsRef::as_ref)).await;
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
