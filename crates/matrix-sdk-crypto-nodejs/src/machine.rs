use napi_derive::napi;
use napi::{Result};
use ruma::{UserId};
use tokio::runtime::Runtime;
use matrix_sdk_crypto::{
    OlmMachine as RSOlmMachine,
};

#[napi]
pub struct SledBackedOlmMachine {
    inner: RSOlmMachine,
    runtime: Runtime,
}

#[napi]
impl SledBackedOlmMachine {
    #[napi(constructor)]
    pub fn new(user_id: String, device_id: String, sled_path: String) -> Result<Self> {
        let user_id = Box::<UserId>::try_from(user_id.as_str()).expect("Failed to parse user ID");
        let device_id = device_id.as_str().into();
        let sled_path = sled_path.as_str();
        let runtime = Runtime::new().expect("Couldn't create a tokio runtime");
        Ok(SledBackedOlmMachine {
            // TODO: Should we be passing a passphrase through?
            inner: runtime.block_on(RSOlmMachine::new_with_default_store(&user_id, device_id, sled_path, None))
                .expect("Failed to create inner Olm machine"),
            runtime,
        })
    }

    #[napi(getter)]
    pub fn user_id(&self) -> String {
        self.inner.user_id().to_string()
    }

    #[napi(getter)]
    pub fn device_id(&self) -> String {
        self.inner.device_id().to_string()
    }

    #[napi]
    pub fn device_display_name(&self) -> Result<Option<String>> {
        Ok(self.runtime.block_on(self.inner.display_name()).expect("Failed to get display name"))
    }
}

