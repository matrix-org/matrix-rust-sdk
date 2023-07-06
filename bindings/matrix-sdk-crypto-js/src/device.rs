//! Types for a `Device`.

use js_sys::{Array, Map, Promise};
use wasm_bindgen::prelude::*;

use crate::{
    encryption::EncryptionAlgorithm,
    future::future_to_promise,
    identifiers::{self, DeviceId, UserId},
    impl_from_to_inner,
    js::try_array_to_vec,
    requests, types, verification, vodozemac,
};

/// A device represents a E2EE capable client of an user.
#[wasm_bindgen]
#[derive(Debug)]
pub struct Device {
    pub(crate) inner: matrix_sdk_crypto::Device,
}

impl_from_to_inner!(matrix_sdk_crypto::Device => Device);

#[wasm_bindgen]
impl Device {
    /// Request an interactive verification with this device.
    ///
    /// Returns a Promise for a 2-element array `[VerificationRequest,
    /// ToDeviceRequest]`.
    #[wasm_bindgen(js_name = "requestVerification")]
    pub fn request_verification(&self, methods: Option<Array>) -> Result<Promise, JsError> {
        let methods =
            methods.map(try_array_to_vec::<verification::VerificationMethod, _>).transpose()?;
        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            let tuple = Array::new();
            let (verification_request, outgoing_verification_request) = match methods {
                Some(methods) => me.request_verification_with_methods(methods).await,
                None => me.request_verification().await,
            };

            tuple.set(0, verification::VerificationRequest::from(verification_request).into());
            tuple.set(
                1,
                verification::OutgoingVerificationRequest::from(outgoing_verification_request)
                    .try_into()?,
            );

            Ok(tuple)
        }))
    }

    /// Is this device considered to be verified.
    ///
    /// This method returns true if either the `is_locally_trusted`
    /// method returns `true` or if the `is_cross_signing_trusted`
    /// method returns `true`.
    #[wasm_bindgen(js_name = "isVerified")]
    pub fn is_verified(&self) -> bool {
        self.inner.is_verified()
    }

    /// Is this device considered to be verified using cross signing.
    #[wasm_bindgen(js_name = "isCrossSigningTrusted")]
    pub fn is_cross_signing_trusted(&self) -> bool {
        self.inner.is_cross_signing_trusted()
    }

    /// Is this device cross-signed by its owner?
    #[wasm_bindgen(js_name = "isCrossSignedByOwner")]
    pub fn is_cross_signed_by_owner(&self) -> bool {
        self.inner.is_cross_signed_by_owner()
    }

    /// Set the local trust state of the device to the given state.
    ///
    /// This won’t affect any cross signing trust state, this only
    /// sets a flag marking to have the given trust state.
    ///
    /// `trust_state` represents the new trust state that should be
    /// set for the device.
    #[wasm_bindgen(js_name = "setLocalTrust")]
    pub fn set_local_trust(&self, local_state: LocalTrust) -> Promise {
        let me = self.inner.clone();

        future_to_promise(async move {
            me.set_local_trust(local_state.into()).await?;

            Ok(JsValue::NULL)
        })
    }

    /// The user ID of the device owner.
    #[wasm_bindgen(getter, js_name = "userId")]
    pub fn user_id(&self) -> UserId {
        self.inner.user_id().to_owned().into()
    }

    /// The unique ID of the device.
    #[wasm_bindgen(getter, js_name = "deviceId")]
    pub fn device_id(&self) -> DeviceId {
        self.inner.device_id().to_owned().into()
    }

    /// Get the human readable name of the device.
    #[wasm_bindgen(getter, js_name = "displayName")]
    pub fn display_name(&self) -> Option<String> {
        self.inner.display_name().map(ToOwned::to_owned)
    }

    /// Get the key of the given key algorithm belonging to this device.
    #[wasm_bindgen(js_name = "getKey")]
    pub fn get_key(
        &self,
        algorithm: identifiers::DeviceKeyAlgorithmName,
    ) -> Result<Option<vodozemac::DeviceKey>, JsError> {
        Ok(self.inner.get_key(algorithm.try_into()?).cloned().map(Into::into))
    }

    /// Get the Curve25519 key of the given device.
    #[wasm_bindgen(getter, js_name = "curve25519Key")]
    pub fn curve25519_key(&self) -> Option<vodozemac::Curve25519PublicKey> {
        self.inner.curve25519_key().map(Into::into)
    }

    /// Get the Ed25519 key of the given device.
    #[wasm_bindgen(getter, js_name = "ed25519Key")]
    pub fn ed25519_key(&self) -> Option<vodozemac::Ed25519PublicKey> {
        self.inner.ed25519_key().map(Into::into)
    }

    /// Get a map containing all the device keys.
    #[wasm_bindgen(getter)]
    pub fn keys(&self) -> Map {
        let map = Map::new();

        for (device_key_id, device_key) in self.inner.keys() {
            map.set(
                &identifiers::DeviceKeyId::from(device_key_id.clone()).into(),
                &vodozemac::DeviceKey::from(device_key.clone()).into(),
            );
        }

        map
    }

    /// Get the list of algorithms this device supports.
    ///
    /// Returns `Array<EncryptionAlgorithm>`.
    #[wasm_bindgen(getter)]
    pub fn algorithms(&self) -> Array {
        self.inner
            .algorithms()
            .iter()
            .map(|alg| EncryptionAlgorithm::from(alg.clone()))
            .map(JsValue::from)
            .collect()
    }

    /// Get a map containing all the device signatures.
    #[wasm_bindgen(getter)]
    pub fn signatures(&self) -> types::Signatures {
        self.inner.signatures().clone().into()
    }

    /// Get the trust state of the device.
    #[wasm_bindgen(getter, js_name = "localTrustState")]
    pub fn local_trust_state(&self) -> LocalTrust {
        self.inner.local_trust_state().into()
    }

    /// Is the device locally marked as trusted?
    #[wasm_bindgen(js_name = "isLocallyTrusted")]
    pub fn is_locally_trusted(&self) -> bool {
        self.inner.is_locally_trusted()
    }

    /// Is the device locally marked as blacklisted?
    ///
    /// Blacklisted devices won’t receive any group sessions.
    #[wasm_bindgen(js_name = "isBlacklisted")]
    pub fn is_blacklisted(&self) -> bool {
        self.inner.is_blacklisted()
    }

    /// Is the device deleted?
    #[wasm_bindgen(js_name = "isDeleted")]
    pub fn is_deleted(&self) -> bool {
        self.inner.is_deleted()
    }

    /// Timestamp representing the first time this device has been seen (in
    /// milliseconds).
    #[wasm_bindgen(js_name = "firstTimeSeen")]
    pub fn first_time_seen(&self) -> u64 {
        self.inner.first_time_seen_ts().0.into()
    }

    /// Mark this device as verified.
    /// Works only if the device is owned by the current user.
    ///
    /// Returns a signature upload request that needs to be sent out.
    pub fn verify(&self) -> Promise {
        let device = self.inner.clone();

        future_to_promise(async move {
            Ok(requests::SignatureUploadRequest::try_from(&device.verify().await?)?)
        })
    }
}

/// The local trust state of a device.
#[wasm_bindgen]
#[derive(Debug)]
pub enum LocalTrust {
    /// The device has been verified and is trusted.
    Verified,

    /// The device been blacklisted from communicating.
    BlackListed,

    /// The trust state of the device is being ignored.
    Ignored,

    /// The trust state is unset.
    Unset,
}

impl From<matrix_sdk_crypto::LocalTrust> for LocalTrust {
    fn from(value: matrix_sdk_crypto::LocalTrust) -> Self {
        use matrix_sdk_crypto::LocalTrust::*;

        match value {
            Verified => Self::Verified,
            BlackListed => Self::BlackListed,
            Ignored => Self::Ignored,
            Unset => Self::Unset,
        }
    }
}

impl From<LocalTrust> for matrix_sdk_crypto::LocalTrust {
    fn from(value: LocalTrust) -> Self {
        use LocalTrust::*;

        match value {
            Verified => Self::Verified,
            BlackListed => Self::BlackListed,
            Ignored => Self::Ignored,
            Unset => Self::Unset,
        }
    }
}

/// A read only view over all devices belonging to a user.
#[wasm_bindgen]
#[derive(Debug)]
pub struct UserDevices {
    pub(crate) inner: matrix_sdk_crypto::UserDevices,
}

impl_from_to_inner!(matrix_sdk_crypto::UserDevices => UserDevices);

#[wasm_bindgen]
impl UserDevices {
    /// Get the specific device with the given device ID.
    pub fn get(&self, device_id: &DeviceId) -> Option<Device> {
        self.inner.get(&device_id.inner).map(Into::into)
    }

    /// Returns true if there is at least one devices of this user
    /// that is considered to be verified, false otherwise.
    ///
    /// This won't consider your own device as verified, as your own
    /// device is always implicitly verified.
    #[wasm_bindgen(js_name = "isAnyVerified")]
    pub fn is_any_verified(&self) -> bool {
        self.inner.is_any_verified()
    }

    /// Array over all the device IDs of the user devices.
    pub fn keys(&self) -> Array {
        self.inner.keys().map(ToOwned::to_owned).map(DeviceId::from).map(JsValue::from).collect()
    }

    /// Iterator over all the devices of the user devices.
    pub fn devices(&self) -> Array {
        self.inner.devices().map(Device::from).map(JsValue::from).collect()
    }
}
