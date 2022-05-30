use js_sys::{Array, JsString};
use ruma::events::key::verification::cancel::CancelCode as RumaCancelCode;
use wasm_bindgen::prelude::*;

use crate::identifiers::{DeviceId, RoomId, UserId};

#[wasm_bindgen]
#[derive(Debug)]
pub struct Sas {
    inner: matrix_sdk_crypto::Sas,
}

#[wasm_bindgen]
impl Sas {
    /// Get our own user ID.
    #[wasm_bindgen(js_name = "userId")]
    pub fn user_id(&self) -> UserId {
        UserId { inner: self.inner.user_id().to_owned() }
    }

    /// Get our own device ID.
    #[wasm_bindgen(js_name = "deviceId")]
    pub fn device_id(&self) -> DeviceId {
        DeviceId { inner: self.inner.device_id().to_owned() }
    }

    /// Get the user id of the other side.
    #[wasm_bindgen(js_name = "otherUserId")]
    pub fn other_user_id(&self) -> UserId {
        UserId { inner: self.inner.other_user_id().to_owned() }
    }

    /// Get the device ID of the other side.
    #[wasm_bindgen(js_name = "otherDeviceId")]
    pub fn other_device_id(&self) -> DeviceId {
        DeviceId { inner: self.inner.other_device_id().to_owned() }
    }

    #[wasm_bindgen(js_name = "otherDevice")]
    pub fn other_device(&self) {
        todo!()
    }

    #[wasm_bindgen(js_name = "flowId")]
    pub fn flow_id(&self) {
        todo!()
    }

    /// Get the room ID if the verification is happening inside a
    /// room.
    #[wasm_bindgen(js_name = "roomId")]
    pub fn room_id(&self) -> Option<RoomId> {
        self.inner.room_id().map(ToOwned::to_owned).map(RoomId::new_with)
    }

    /// Does this verification flow support displaying emoji for the
    /// short authentication string.
    #[wasm_bindgen(js_name = "supportsEmoji")]
    pub fn supports_emoji(&self) -> bool {
        self.inner.supports_emoji()
    }

    /// Did this verification flow start from a verification request.
    #[wasm_bindgen(js_name = "startedFromRequest")]
    pub fn started_from_request(&self) -> bool {
        self.inner.started_from_request()
    }

    /// Is this a verification that is veryfying one of our own
    /// devices.
    #[wasm_bindgen(js_name = "isSelfVerification")]
    pub fn is_self_verification(&self) -> bool {
        self.inner.is_self_verification()
    }

    /// Have we confirmed that the short auth string matches.
    #[wasm_bindgen(js_name = "haveWeConfirmed")]
    pub fn have_we_confirmed(&self) -> bool {
        self.inner.have_we_confirmed()
    }

    /// Has the verification been accepted by both parties.
    #[wasm_bindgen(js_name = "hasBeenAccepted")]
    pub fn has_been_accepted(&self) -> bool {
        self.inner.has_been_accepted()
    }

    /// Get info about the cancellation if the verification flow has
    /// been cancelled.
    #[wasm_bindgen(js_name = "cancelInfo")]
    pub fn cancel_info(&self) -> Option<CancelInfo> {
        self.inner.cancel_info().map(CancelInfo::new_with)
    }

    /// Did we initiate the verification flow.
    #[wasm_bindgen(js_name = "weStarted")]
    pub fn we_started(&self) -> bool {
        self.inner.we_started()
    }

    pub fn accept(&self) {
        todo!()
    }

    #[wasm_bindgen(js_name = "acceptWithSettings")]
    pub fn accept_with_settings(&self) {
        todo!()
    }

    pub fn confirm(&self) {
        todo!()
    }

    pub fn cancel(&self) {
        todo!()
    }

    #[wasm_bindgen(js_name = "cancelWithCode")]
    pub fn cancel_with_code(&self) {
        todo!()
    }

    /// Has the SAS verification flow timed out.
    #[wasm_bindgen(js_name = "timedOut")]
    pub fn timed_out(&self) -> bool {
        self.inner.timed_out()
    }

    /// Are we in a state where we can show the short auth string.
    #[wasm_bindgen(js_name = "canBePresented")]
    pub fn can_be_presented(&self) -> bool {
        self.inner.can_be_presented()
    }

    /// Is the SAS flow done.
    #[wasm_bindgen(js_name = "isDone")]
    pub fn is_done(&self) -> bool {
        self.inner.is_done()
    }

    /// Is the SAS flow canceled.
    #[wasm_bindgen(js_name = "isCancelled")]
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    /// Get the emoji version of the short auth string.
    ///
    /// Returns `undefined` if we can't yet present the short auth string,
    /// otherwise seven tuples containing the emoji and description.
    pub fn emoji(&self) -> Option<Array> {
        Some(
            self.inner
                .emoji()?
                .iter()
                .map(|emoji| Emoji::new_with(emoji.clone()))
                .map(JsValue::from)
                .collect(),
        )
    }

    /// Get the index of the emoji representing the short auth string
    ///
    /// Returns `undefined` if we can’t yet present the short auth
    /// string, otherwise seven u8 numbers in the range from 0 to 63
    /// inclusive which can be converted to an emoji using [the
    /// relevant specification
    /// entry](https://spec.matrix.org/unstable/client-server-api/#sas-method-emoji).
    #[wasm_bindgen(js_name = "emoji_index")]
    pub fn emoji_index(&self) -> Option<Array> {
        Some(self.inner.emoji_index()?.iter().map(|emoji| *emoji).map(JsValue::from).collect())
    }

    /// Get the decimal version of the short auth string.
    ///
    /// Returns None if we can’t yet present the short auth string,
    /// otherwise a tuple containing three 4-digit integers that
    /// represent the short auth string.
    pub fn decimals(&self) -> Option<Array> {
        let decimals = self.inner.decimals()?;

        let out = Array::new_with_length(3);
        out.set(0, JsValue::from(decimals.0));
        out.set(1, JsValue::from(decimals.1));
        out.set(2, JsValue::from(decimals.2));

        Some(out)
    }
}

#[cfg(feature = "qrcode")]
#[wasm_bindgen]
#[derive(Debug)]
pub struct Qr {
    inner: matrix_sdk_crypto::QrVerification,
}

#[cfg(feature = "qrcode")]
#[wasm_bindgen]
impl Qr {
    #[wasm_bindgen(js_name = "hasBeenScanned")]
    pub fn has_been_scanned(&self) -> bool {
        self.inner.has_been_scanned()
    }
}

pub(crate) struct Verification(pub(crate) matrix_sdk_crypto::Verification);

impl TryFrom<Verification> for JsValue {
    type Error = JsError;

    fn try_from(verification: Verification) -> Result<Self, Self::Error> {
        use matrix_sdk_crypto::Verification::*;

        Ok(match verification.0 {
            SasV1(sas) => JsValue::from(Sas { inner: sas }),

            #[cfg(feature = "qrcode")]
            QrV1(qr) => JsValue::from(Qr { inner: qr }),

            _ => {
                return Err(JsError::new(
                    "Unknown verification type, expect `m.sas.v1` only for now",
                ))
            }
        })
    }
}

#[wasm_bindgen]
pub struct CancelInfo {
    inner: matrix_sdk_crypto::CancelInfo,
}

impl CancelInfo {
    pub(crate) fn new_with(inner: matrix_sdk_crypto::CancelInfo) -> Self {
        Self { inner }
    }
}

#[wasm_bindgen]
impl CancelInfo {
    pub fn reason(&self) -> JsString {
        self.inner.reason().into()
    }

    #[wasm_bindgen(js_name = "cancelCode")]
    pub fn cancel_code(&self) -> CancelCode {
        self.inner.cancel_code().into()
    }

    #[wasm_bindgen(js_name = "cancelledbyUs")]
    pub fn cancelled_by_us(&self) -> bool {
        self.inner.cancelled_by_us()
    }
}

#[wasm_bindgen]
pub enum CancelCode {
    /// Unknown cancel code.
    Other,

    /// The user cancelled the verification.
    User,

    /// The verification process timed out.
    ///
    /// Verification processes can define their own timeout
    /// parameters.
    Timeout,

    /// The device does not know about the given transaction ID.
    UnknownTransaction,

    /// The device does not know how to handle the requested method.
    ///
    /// Should be sent for `m.key.verification.start` messages and
    /// messages defined by individual verification processes.
    UnknownMethod,

    /// The device received an unexpected message.
    ///
    /// Typically raised when one of the parties is handling the
    /// verification out of order.
    UnexpectedMessage,

    /// The key was not verified.
    KeyMismatch,

    /// The expected user did not match the user verified.
    UserMismatch,

    /// The message received was invalid.
    InvalidMessage,

    /// An `m.key.verification.request` was accepted by a different
    /// device.
    ///
    /// The device receiving this error can ignore the verification
    /// request.
    Accepted,

    /// The device receiving this error can ignore the verification
    /// request.
    MismatchedCommitment,

    /// The SAS did not match.
    MismatchedSas,
}

impl From<&RumaCancelCode> for CancelCode {
    fn from(code: &RumaCancelCode) -> Self {
        use RumaCancelCode::*;

        match code {
            User => Self::User,
            Timeout => Self::Timeout,
            UnknownTransaction => Self::UnknownTransaction,
            UnknownMethod => Self::UnknownMethod,
            UnexpectedMessage => Self::UnexpectedMessage,
            KeyMismatch => Self::KeyMismatch,
            UserMismatch => Self::UserMismatch,
            InvalidMessage => Self::InvalidMessage,
            Accepted => Self::Accepted,
            MismatchedCommitment => Self::MismatchedCommitment,
            MismatchedSas => Self::MismatchedSas,
            _ => Self::Other,
        }
    }
}

#[wasm_bindgen]
pub struct Emoji {
    inner: matrix_sdk_crypto::Emoji,
}

impl Emoji {
    pub(crate) fn new_with(inner: matrix_sdk_crypto::Emoji) -> Self {
        Self { inner }
    }
}

#[wasm_bindgen]
impl Emoji {
    #[wasm_bindgen(getter)]
    pub fn symbol(&self) -> JsString {
        self.inner.symbol.into()
    }

    #[wasm_bindgen(getter)]
    pub fn description(&self) -> JsString {
        self.inner.description.into()
    }
}
