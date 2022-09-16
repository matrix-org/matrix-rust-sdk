//! User identities.

use js_sys::{Array, Promise};
use wasm_bindgen::prelude::*;

use crate::{
    future::future_to_promise, identifiers, impl_from_to_inner, js::try_array_to_vec, requests,
    verification,
};

pub(crate) struct UserIdentities {
    inner: matrix_sdk_crypto::UserIdentities,
}

impl_from_to_inner!(matrix_sdk_crypto::UserIdentities => UserIdentities);

impl From<UserIdentities> for JsValue {
    fn from(user_identities: UserIdentities) -> Self {
        use matrix_sdk_crypto::UserIdentities::*;

        match user_identities.inner {
            Own(own) => JsValue::from(OwnUserIdentity::from(own)),
            Other(other) => JsValue::from(UserIdentity::from(other)),
        }
    }
}

#[wasm_bindgen]
#[derive(Debug)]
pub struct OwnUserIdentity {
    inner: matrix_sdk_crypto::OwnUserIdentity,
}

impl_from_to_inner!(matrix_sdk_crypto::OwnUserIdentity => OwnUserIdentity);

#[wasm_bindgen]
#[derive(Debug)]
pub struct UserIdentity {
    inner: matrix_sdk_crypto::UserIdentity,
}

impl_from_to_inner!(matrix_sdk_crypto::UserIdentity => UserIdentity);

#[wasm_bindgen]
impl UserIdentity {
    /// Is this user identity verified?
    #[wasm_bindgen(js_name = "isVerified")]
    pub fn is_verified(&self) -> bool {
        self.inner.is_verified()
    }

    /// Manually verify this user.
    ///
    /// This method will attempt to sign the user identity using our private
    /// cross signing key.
    ///
    /// This method fails if we don't have the private part of our user-signing
    /// key.
    ///
    /// Returns a request that needs to be sent out for the user to be marked as
    /// verified.
    pub fn verify(&self) -> Promise {
        let me = self.inner.clone();

        future_to_promise(async move {
            Ok(requests::SignatureUploadRequest::try_from(&me.verify().await?)?)
        })
    }

    /// Create a `VerificationRequest` object after the verification
    /// request content has been sent out.  }
    #[wasm_bindgen(js_name = "requestVerification")]
    pub fn request_verification(
        &self,
        room_id: &identifiers::RoomId,
        request_event_id: &identifiers::EventId,
        methods: Option<Array>,
    ) -> Result<Promise, JsError> {
        let me = self.inner.clone();
        let room_id = room_id.inner.clone();
        let request_event_id = request_event_id.inner.clone();
        let methods =
            methods.map(try_array_to_vec::<verification::VerificationMethod, _>).transpose()?;

        Ok(future_to_promise::<_, verification::VerificationRequest>(async move {
            Ok(me
                .request_verification(room_id.as_ref(), request_event_id.as_ref(), methods)
                .await
                .into())
        }))
    }
}
