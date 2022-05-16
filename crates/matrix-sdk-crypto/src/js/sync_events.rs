//! `GET /_matrix/client/*/sync`

use js_sys::Array;
use wasm_bindgen::prelude::*;

use crate::js::identifiers;

/// Information on E2E device updates.
#[wasm_bindgen]
#[derive(Debug)]
pub struct DeviceLists {
    pub(crate) inner: ruma::api::client::sync::sync_events::v3::DeviceLists,
}

#[wasm_bindgen]
impl DeviceLists {
    /// Create an empty `DeviceLists`.
    ///
    /// `changed` and `left` must be an array of strings representing
    /// a user ID, otherwise invalid entries will be filtered out with
    /// an error.
    #[wasm_bindgen(constructor)]
    pub fn new(changed: Array, left: Array) -> Result<DeviceLists, JsError> {
        let mut inner = ruma::api::client::sync::sync_events::v3::DeviceLists::default();

        inner.changed = changed
            .iter()
            .map(|user| {
                let user = user
                    .as_string()
                    .ok_or_else(|| JsError::new("Given user ID is not a string"))?;
                let user = ruma::UserId::parse(&user).map_err(|error| {
                    JsError::new(&format!(
                        "Given user ID `{}` has an invalid syntax: {}",
                        user, error,
                    ))
                })?;

                Ok(user)
            })
            .collect::<Result<Vec<ruma::OwnedUserId>, JsError>>()?;

        inner.left = left
            .iter()
            .map(|user| {
                let user = user
                    .as_string()
                    .ok_or_else(|| JsError::new("Given user ID is not a string"))?;
                let user = ruma::UserId::parse(&user).map_err(|error| {
                    JsError::new(&format!(
                        "Given user ID `{}` has an invalid syntax: {}",
                        user, error,
                    ))
                })?;

                Ok(user)
            })
            .collect::<Result<Vec<ruma::OwnedUserId>, JsError>>()?;

        Ok(Self { inner })
    }

    /// Returns true if there are no device list updates.
    #[wasm_bindgen(js_name = "isEmpty")]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// List of users who have updated their device identity keys or who now
    /// share an encrypted room with the client since the previous sync
    pub fn changed(&self) -> Array {
        self.inner
            .changed
            .iter()
            .map(|user| identifiers::UserId { inner: user.clone() })
            .map(JsValue::from)
            .collect()
    }

    /// List of users who no longer share encrypted rooms since the previous
    /// sync response.
    pub fn left(&self) -> Array {
        self.inner
            .left
            .iter()
            .map(|user| identifiers::UserId { inner: user.clone() })
            .map(JsValue::from)
            .collect()
    }
}
