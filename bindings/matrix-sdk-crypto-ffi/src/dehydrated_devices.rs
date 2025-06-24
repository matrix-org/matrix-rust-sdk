use std::{mem::ManuallyDrop, sync::Arc};

use matrix_sdk_common::executor::Handle;
use matrix_sdk_crypto::{
    dehydrated_devices::{
        DehydratedDevice as InnerDehydratedDevice, DehydratedDevices as InnerDehydratedDevices,
        RehydratedDevice as InnerRehydratedDevice,
    },
    store::types::DehydratedDeviceKey as InnerDehydratedDeviceKey,
    DecryptionSettings,
};
use ruma::{api::client::dehydrated_device, events::AnyToDeviceEvent, serde::Raw, OwnedDeviceId};
use serde_json::json;

use crate::{CryptoStoreError, DehydratedDeviceKey};

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum DehydrationError {
    #[error(transparent)]
    Pickle(#[from] matrix_sdk_crypto::vodozemac::DehydratedDeviceError),
    #[error(transparent)]
    LegacyPickle(#[from] matrix_sdk_crypto::vodozemac::LibolmPickleError),
    #[error(transparent)]
    MissingSigningKey(#[from] matrix_sdk_crypto::SignatureError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Store(#[from] matrix_sdk_crypto::CryptoStoreError),
    #[error("The pickle key has an invalid length, expected 32 bytes, got {0}")]
    PickleKeyLength(usize),
    #[error(transparent)]
    Rand(#[from] rand::Error),
}

impl From<matrix_sdk_crypto::dehydrated_devices::DehydrationError> for DehydrationError {
    fn from(value: matrix_sdk_crypto::dehydrated_devices::DehydrationError) -> Self {
        match value {
            matrix_sdk_crypto::dehydrated_devices::DehydrationError::Json(e) => Self::Json(e),
            matrix_sdk_crypto::dehydrated_devices::DehydrationError::Pickle(e) => Self::Pickle(e),
            matrix_sdk_crypto::dehydrated_devices::DehydrationError::LegacyPickle(e) => {
                Self::LegacyPickle(e)
            }
            matrix_sdk_crypto::dehydrated_devices::DehydrationError::MissingSigningKey(e) => {
                Self::MissingSigningKey(e)
            }
            matrix_sdk_crypto::dehydrated_devices::DehydrationError::Store(e) => Self::Store(e),
            matrix_sdk_crypto::dehydrated_devices::DehydrationError::PickleKeyLength(l) => {
                Self::PickleKeyLength(l)
            }
        }
    }
}

#[derive(uniffi::Object)]
pub struct DehydratedDevices {
    pub(crate) runtime: Handle,
    pub(crate) inner: ManuallyDrop<InnerDehydratedDevices>,
}

impl Drop for DehydratedDevices {
    fn drop(&mut self) {
        // See the drop implementation for the `crate::OlmMachine` for an explanation.
        let _guard = self.runtime.enter();
        unsafe {
            ManuallyDrop::drop(&mut self.inner);
        }
    }
}

#[matrix_sdk_ffi_macros::export]
impl DehydratedDevices {
    pub fn create(&self) -> Result<Arc<DehydratedDevice>, DehydrationError> {
        let inner = self.runtime.block_on(self.inner.create())?;

        Ok(Arc::new(DehydratedDevice {
            inner: ManuallyDrop::new(inner),
            runtime: self.runtime.to_owned(),
        }))
    }

    pub fn rehydrate(
        &self,
        pickle_key: &DehydratedDeviceKey,
        device_id: String,
        device_data: String,
    ) -> Result<Arc<RehydratedDevice>, DehydrationError> {
        let device_data: Raw<_> = serde_json::from_str(&device_data)?;
        let device_id: OwnedDeviceId = device_id.into();

        let key = InnerDehydratedDeviceKey::from_slice(&pickle_key.inner)?;

        let ret = RehydratedDevice {
            runtime: self.runtime.to_owned(),
            inner: ManuallyDrop::new(self.runtime.block_on(self.inner.rehydrate(
                &key,
                &device_id,
                device_data,
            ))?),
        }
        .into();

        Ok(ret)
    }

    /// Get the cached dehydrated device pickle key if any.
    ///
    /// None if the key was not previously cached (via
    /// [`Self::save_dehydrated_device_pickle_key`]).
    ///
    /// Should be used to periodically rotate the dehydrated device to avoid
    /// OTK exhaustion and accumulation of to_device messages.
    pub fn get_dehydrated_device_key(
        &self,
    ) -> Result<Option<crate::DehydratedDeviceKey>, CryptoStoreError> {
        Ok(self
            .runtime
            .block_on(self.inner.get_dehydrated_device_pickle_key())?
            .map(crate::DehydratedDeviceKey::from))
    }

    /// Store the dehydrated device pickle key in the crypto store.
    ///
    /// This is useful if the client wants to periodically rotate dehydrated
    /// devices to avoid OTK exhaustion and accumulated to_device problems.
    pub fn save_dehydrated_device_key(
        &self,
        pickle_key: &crate::DehydratedDeviceKey,
    ) -> Result<(), CryptoStoreError> {
        let pickle_key = InnerDehydratedDeviceKey::from_slice(&pickle_key.inner)?;
        Ok(self.runtime.block_on(self.inner.save_dehydrated_device_pickle_key(&pickle_key))?)
    }

    /// Deletes the previously stored dehydrated device pickle key.
    pub fn delete_dehydrated_device_key(&self) -> Result<(), CryptoStoreError> {
        Ok(self.runtime.block_on(self.inner.delete_dehydrated_device_pickle_key())?)
    }
}

#[derive(uniffi::Object)]
pub struct RehydratedDevice {
    inner: ManuallyDrop<InnerRehydratedDevice>,
    runtime: Handle,
}

impl Drop for RehydratedDevice {
    fn drop(&mut self) {
        // See the drop implementation for the `crate::OlmMachine` for an explanation.
        let _guard = self.runtime.enter();
        unsafe {
            ManuallyDrop::drop(&mut self.inner);
        }
    }
}

#[matrix_sdk_ffi_macros::export]
impl RehydratedDevice {
    pub fn receive_events(
        &self,
        events: String,
        decryption_settings: &DecryptionSettings,
    ) -> Result<(), crate::CryptoStoreError> {
        let events: Vec<Raw<AnyToDeviceEvent>> = serde_json::from_str(&events)?;
        self.runtime.block_on(self.inner.receive_events(events, decryption_settings))?;

        Ok(())
    }
}

#[derive(uniffi::Object)]
pub struct DehydratedDevice {
    pub(crate) runtime: Handle,
    pub(crate) inner: ManuallyDrop<InnerDehydratedDevice>,
}

impl Drop for DehydratedDevice {
    fn drop(&mut self) {
        // See the drop implementation for the `crate::OlmMachine` for an explanation.
        let _guard = self.runtime.enter();
        unsafe {
            ManuallyDrop::drop(&mut self.inner);
        }
    }
}

#[matrix_sdk_ffi_macros::export]
impl DehydratedDevice {
    pub fn keys_for_upload(
        &self,
        device_display_name: String,
        pickle_key: &DehydratedDeviceKey,
    ) -> Result<UploadDehydratedDeviceRequest, DehydrationError> {
        let key = InnerDehydratedDeviceKey::from_slice(&pickle_key.inner)?;

        let request =
            self.runtime.block_on(self.inner.keys_for_upload(device_display_name, &key))?;

        Ok(request.into())
    }
}

#[derive(Debug, uniffi::Record)]
pub struct UploadDehydratedDeviceRequest {
    /// The serialized JSON body of the request.
    body: String,
}

impl From<dehydrated_device::put_dehydrated_device::unstable::Request>
    for UploadDehydratedDeviceRequest
{
    fn from(value: dehydrated_device::put_dehydrated_device::unstable::Request) -> Self {
        let body = json!({
            "device_id": value.device_id,
            "device_data": value.device_data,
            "initial_device_display_name": value.initial_device_display_name,
            "device_keys": value.device_keys,
            "one_time_keys": value.one_time_keys,
            "fallback_keys": value.fallback_keys,
        });

        let body = serde_json::to_string(&body)
            .expect("We should be able to serialize the PUT dehydrated devices request body");

        Self { body }
    }
}

#[cfg(test)]
mod tests {
    use crate::{dehydrated_devices::DehydrationError, DehydratedDeviceKey};

    #[test]
    fn test_creating_dehydrated_key() {
        let result = DehydratedDeviceKey::new();
        assert!(result.is_ok());
        let dehydrated_device_key = result.unwrap();
        let base_64 = dehydrated_device_key.to_base64();
        let inner_bytes = dehydrated_device_key.inner;

        let copy = DehydratedDeviceKey::from_slice(&inner_bytes).unwrap();

        assert_eq!(base_64, copy.to_base64());
    }

    #[test]
    fn test_creating_dehydrated_key_failure() {
        let bytes = [0u8; 24];

        let pickle_key = DehydratedDeviceKey::from_slice(&bytes);

        assert!(pickle_key.is_err());

        match pickle_key {
            Err(DehydrationError::PickleKeyLength(pickle_key_length)) => {
                assert_eq!(bytes.len(), pickle_key_length);
            }
            _ => panic!("Should have failed!"),
        }
    }
}
