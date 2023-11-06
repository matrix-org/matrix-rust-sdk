use std::{mem::ManuallyDrop, sync::Arc};

use matrix_sdk_crypto::dehydrated_devices::{
    DehydratedDevice as InnerDehydratedDevice, DehydratedDevices as InnerDehydratedDevices,
    RehydratedDevice as InnerRehydratedDevice,
};
use ruma::{api::client::dehydrated_device, events::AnyToDeviceEvent, serde::Raw, OwnedDeviceId};
use serde_json::json;
use tokio::runtime::Handle;
use zeroize::Zeroize;

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum DehydrationError {
    #[error(transparent)]
    Pickle(#[from] matrix_sdk_crypto::vodozemac::LibolmPickleError),
    #[error(transparent)]
    MissingSigningKey(#[from] matrix_sdk_crypto::SignatureError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Store(#[from] matrix_sdk_crypto::CryptoStoreError),
    #[error("The pickle key has an invalid length, expected 32 bytes, got {0}")]
    PickleKeyLength(usize),
}

impl From<matrix_sdk_crypto::dehydrated_devices::DehydrationError> for DehydrationError {
    fn from(value: matrix_sdk_crypto::dehydrated_devices::DehydrationError) -> Self {
        match value {
            matrix_sdk_crypto::dehydrated_devices::DehydrationError::Json(e) => Self::Json(e),
            matrix_sdk_crypto::dehydrated_devices::DehydrationError::Pickle(e) => Self::Pickle(e),
            matrix_sdk_crypto::dehydrated_devices::DehydrationError::MissingSigningKey(e) => {
                Self::MissingSigningKey(e)
            }
            matrix_sdk_crypto::dehydrated_devices::DehydrationError::Store(e) => Self::Store(e),
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

#[uniffi::export]
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
        pickle_key: Vec<u8>,
        device_id: String,
        device_data: String,
    ) -> Result<Arc<RehydratedDevice>, DehydrationError> {
        let device_data: Raw<_> = serde_json::from_str(&device_data)?;
        let device_id: OwnedDeviceId = device_id.into();

        let mut key = get_pickle_key(&pickle_key)?;

        let ret = RehydratedDevice {
            runtime: self.runtime.to_owned(),
            inner: ManuallyDrop::new(self.runtime.block_on(self.inner.rehydrate(
                &key,
                &device_id,
                device_data,
            ))?),
        }
        .into();

        key.zeroize();

        Ok(ret)
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

#[uniffi::export]
impl RehydratedDevice {
    pub fn receive_events(&self, events: String) -> Result<(), crate::CryptoStoreError> {
        let events: Vec<Raw<AnyToDeviceEvent>> = serde_json::from_str(&events)?;
        self.runtime.block_on(self.inner.receive_events(events))?;

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

#[uniffi::export]
impl DehydratedDevice {
    pub fn keys_for_upload(
        &self,
        device_display_name: String,
        pickle_key: Vec<u8>,
    ) -> Result<UploadDehydratedDeviceRequest, DehydrationError> {
        let mut key = get_pickle_key(&pickle_key)?;

        let request =
            self.runtime.block_on(self.inner.keys_for_upload(device_display_name, &key))?;

        key.zeroize();

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

fn get_pickle_key(pickle_key: &[u8]) -> Result<Box<[u8; 32]>, DehydrationError> {
    let pickle_key_length = pickle_key.len();

    if pickle_key_length == 32 {
        let mut key = Box::new([0u8; 32]);
        key.copy_from_slice(pickle_key);

        Ok(key)
    } else {
        Err(DehydrationError::PickleKeyLength(pickle_key_length))
    }
}
