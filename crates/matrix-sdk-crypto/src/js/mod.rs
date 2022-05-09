//! Additional API that can be useful from JavaScript.

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
#[derive(Debug, Clone)]
pub struct UserId {
    pub(crate) inner: ruma::OwnedUserId,
}

#[wasm_bindgen]
impl UserId {
    #[wasm_bindgen(constructor)]
    pub fn new(id: &str) -> Result<UserId, String> {
        Ok(Self { inner: ruma::UserId::parse(id).map_err(|e| e.to_string())? })
    }

    pub fn localpart(&self) -> String {
        self.inner.localpart().to_owned()
    }

    #[wasm_bindgen(getter, js_name = "isHistorical")]
    pub fn is_historical(&self) -> bool {
        self.inner.is_historical()
    }
}

#[wasm_bindgen]
#[derive(Debug, Clone)]
pub struct DeviceId {
    pub(crate) inner: ruma::OwnedDeviceId,
}

#[wasm_bindgen]
impl DeviceId {
    #[wasm_bindgen(constructor)]
    pub fn new(id: &str) -> DeviceId {
        Self { inner: id.into() }
    }
}
