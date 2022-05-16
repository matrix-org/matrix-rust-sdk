//! Types for [Matrix](https://matrix.org/) identifiers for devices,
//! events, keys, rooms, servers, users and URIs.

use wasm_bindgen::prelude::*;

/// A Matrix [user ID].
///
/// [user ID]: https://spec.matrix.org/v1.2/appendices/#user-identifiers
#[wasm_bindgen]
#[derive(Debug, Clone)]
pub struct UserId {
    pub(crate) inner: ruma::OwnedUserId,
}

#[wasm_bindgen]
impl UserId {
    /// Parse/validate and create a new `UserId`.
    #[wasm_bindgen(constructor)]
    pub fn new(id: &str) -> Result<UserId, JsError> {
        Ok(Self { inner: ruma::UserId::parse(id)? })
    }

    /// Returns the user's localpart.
    pub fn localpart(&self) -> String {
        self.inner.localpart().to_owned()
    }

    /// Whether this user ID is a historical one.
    ///
    /// A historical user ID is one that doesn't conform to the latest
    /// specification of the user ID grammar but is still accepted
    /// because it was previously allowed.
    #[wasm_bindgen(getter, js_name = "isHistorical")]
    pub fn is_historical(&self) -> bool {
        self.inner.is_historical()
    }
}

/// A Matrix key ID.
///
/// Device identifiers in Matrix are completely opaque character
/// sequences. This type is provided simply for its semantic value.
#[wasm_bindgen]
#[derive(Debug, Clone)]
pub struct DeviceId {
    pub(crate) inner: ruma::OwnedDeviceId,
}

#[wasm_bindgen]
impl DeviceId {
    /// Create a new `DeviceId`.
    #[wasm_bindgen(constructor)]
    pub fn new(id: &str) -> DeviceId {
        Self { inner: id.into() }
    }
}
