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

/// Struct representing a cross signing identity of a user.
///
/// This is the user identity of a user that is our own.
#[wasm_bindgen]
#[derive(Debug)]
pub struct OwnUserIdentity {
    inner: matrix_sdk_crypto::OwnUserIdentity,
}

impl_from_to_inner!(matrix_sdk_crypto::OwnUserIdentity => OwnUserIdentity);

#[wasm_bindgen]
impl OwnUserIdentity {
    /// Mark our user identity as verified.
    ///
    /// This will mark the identity locally as verified and sign it with our own
    /// device.
    ///
    /// Returns a signature upload request that needs to be sent out.
    pub fn verify(&self) -> Promise {
        let me = self.inner.clone();

        future_to_promise(async move {
            Ok(requests::SignatureUploadRequest::try_from(&me.verify().await?)?)
        })
    }

    /// Send a verification request to our other devices.
    #[wasm_bindgen(js_name = "requestVerification")]
    pub fn request_verification(&self, methods: Option<Array>) -> Result<Promise, JsError> {
        let methods =
            methods.map(try_array_to_vec::<verification::VerificationMethod, _>).transpose()?;
        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            let tuple = Array::new();
            let (verification_request, outgoing_verification_request) = match methods {
                Some(methods) => me.request_verification_with_methods(methods).await?,
                None => me.request_verification().await?,
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

    /// Does our user identity trust our own device, i.e. have we signed our own
    /// device keys with our self-signing key?
    #[wasm_bindgen(js_name = "trustsOurOwnDevice")]
    pub fn trusts_our_own_device(&self) -> Promise {
        let me = self.inner.clone();

        future_to_promise(async move { Ok(me.trusts_our_own_device().await?) })
    }

    /// Get the master key of the identity.
    #[wasm_bindgen(getter, js_name = "masterKey")]
    pub fn master_key(&self) -> Result<String, JsError> {
        let master_key = self.inner.master_key().as_ref();
        Ok(serde_json::to_string(master_key)?)
    }

    /// Get the self-signing key of the identity.
    #[wasm_bindgen(getter, js_name = "selfSigningKey")]
    pub fn self_signing_key(&self) -> Result<String, JsError> {
        let self_signing_key = self.inner.self_signing_key().as_ref();
        Ok(serde_json::to_string(self_signing_key)?)
    }

    /// Get the user-signing key of the identity, this is only present for our
    /// own user identity..
    #[wasm_bindgen(getter, js_name = "userSigningKey")]
    pub fn user_signing_key(&self) -> Result<String, JsError> {
        let user_signing_key = self.inner.user_signing_key().as_ref();
        Ok(serde_json::to_string(user_signing_key)?)
    }
}

/// Struct representing a cross signing identity of a user.
///
/// This is the user identity of a user that isn't our own. Other users will
/// only contain a master key and a self signing key, meaning that only device
/// signatures can be checked with this identity.
///
/// This struct wraps a read-only version of the struct and allows verifications
/// to be requested to verify our own device with the user identity.
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
    /// request content has been sent out.
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

    /// Send a verification request to the given user.
    ///
    /// The returned content needs to be sent out into a DM room with the given
    /// user.
    ///
    /// After the content has been sent out a VerificationRequest can be started
    /// with the `request_verification` method.
    #[wasm_bindgen(js_name = "verificationRequestContent")]
    pub fn verification_request_content(&self, methods: Option<Array>) -> Result<Promise, JsError> {
        let me = self.inner.clone();
        let methods =
            methods.map(try_array_to_vec::<verification::VerificationMethod, _>).transpose()?;

        Ok(future_to_promise(async move {
            Ok(serde_json::to_string(&me.verification_request_content(methods).await)?)
        }))
    }

    /// Get the master key of the identity.
    #[wasm_bindgen(getter, js_name = "masterKey")]
    pub fn master_key(&self) -> Result<String, JsError> {
        let master_key = self.inner.master_key().as_ref();
        Ok(serde_json::to_string(master_key)?)
    }

    /// Get the self-signing key of the identity.
    #[wasm_bindgen(getter, js_name = "selfSigningKey")]
    pub fn self_signing_key(&self) -> Result<String, JsError> {
        let self_signing_key = self.inner.self_signing_key().as_ref();
        Ok(serde_json::to_string(self_signing_key)?)
    }
}
