// Copyright 2024 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Data types for the QR code login mechanism described in [MSC4108] and
//! [MSC4388].
//!
//! [MSC4108]: https://github.com/matrix-org/matrix-spec-proposals/pull/4108
//! [MSC4388]: https://github.com/matrix-org/matrix-spec-proposals/pull/4388

use std::str::Utf8Error;

use thiserror::Error;

mod msc_4108;
mod msc_4388;

pub use msc_4108::Msc4108IntentData;
use url::Url;
use vodozemac::{Curve25519PublicKey, base64_decode, base64_encode};

/// Error type for the decoding of the [`QrCodeData`].
#[derive(Debug, Error)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Error), uniffi(flat_error))]
pub enum LoginQrCodeDecodeError {
    /// The QR code data is no long enough, it's missing some fields.
    #[error("The QR code data is missing some fields.")]
    NotEnoughData(#[from] std::io::Error),
    /// One of the URLs in the QR code data is not a valid UTF-8 encoded string.
    #[error("One of the URLs in the QR code data is not a valid UTF-8 string")]
    NotUtf8(#[from] Utf8Error),
    /// One of the URLs in the QR code data could not be parsed.
    #[error("One of the URLs in the QR code data could not be parsed: {0:?}")]
    UrlParse(#[from] url::ParseError),
    /// The QR code data contains an invalid intent, we expect the login
    /// intent or the reciprocate intent.
    #[error(
        "The QR code data contains an invalid QR code intent, expected {expected_login} or {expected_reciprocate}, got {got}"
    )]
    InvalidIntent {
        /// The constant we expect for the login intent.
        expected_login: u8,
        /// The constant we expect for the reciprocate intent.
        expected_reciprocate: u8,
        /// The intent we received.
        got: u8,
    },
    /// The QR code data contains an unsupported type.
    #[error("The QR code data contains an unsupported type, expected {expected}, got {got}")]
    InvalidType {
        /// The type we expected.
        expected: u8,
        /// The type we received.
        got: u8,
    },
    /// The base64 encoded variant of the QR code data is not a valid base64
    /// string.
    #[error("The QR code data could not have been decoded from a base64 string: {0:?}")]
    Base64(#[from] vodozemac::Base64DecodeError),
    /// The QR code data doesn't contain the expected `MATRIX` prefix.
    #[error("The QR code data has an unexpected prefix, expected: {expected:?}, got {got:?}")]
    InvalidPrefix {
        /// The expected prefix.
        expected: &'static [u8],
        /// The prefix we received.
        got: Vec<u8>,
    },
}

/// Intent-specific data of the [`QrCodeData`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QrCodeIntentData<'a> {
    /// Intent-specific data in the case the QR code adheres to [MSC4108] of the
    /// QR code data format.
    ///
    /// [MSC4108]: https://github.com/matrix-org/matrix-spec-proposals/pull/4108
    Msc4108 {
        /// Intent specific data for the MSC4108 variant.
        data: &'a Msc4108IntentData,
        /// The rendezvous URL for the MSC4108 variant of the rendezvous
        /// channel.
        rendezvous_url: &'a Url,
    },
    /// Intent-specific data in the case the QR code adheres to [MSC4388] of the
    /// QR code data format.
    ///
    /// [MSC4388]: https://github.com/matrix-org/matrix-spec-proposals/pull/4388
    Msc4388 {
        /// The ID of the rendezvous session, can be used to exchange messages
        /// with the other device.
        rendezvous_id: &'a str,
        /// The base URL of the homeserver that the device generating the QR is
        /// using.
        base_url: &'a Url,
    },
}

/// The intent of the device that generated/displayed the QR code.
///
/// The QR code login mechanism supports both, the new device, as well as the
/// existing device to display the QR code.
///
/// The different intents have an explicit one-byte identifier which gets added
/// to the QR code data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QrCodeIntent {
    /// Enum variant for the case where the new device is displaying the QR
    /// code.
    Login,
    /// Enum variant for the case where the existing device is displaying the QR
    /// code.
    Reciprocate,
}

impl From<msc_4108::QrCodeIntent> for QrCodeIntent {
    fn from(value: msc_4108::QrCodeIntent) -> Self {
        match value {
            msc_4108::QrCodeIntent::Login => Self::Login,
            msc_4108::QrCodeIntent::Reciprocate => Self::Reciprocate,
        }
    }
}

impl From<msc_4388::QrCodeIntent> for QrCodeIntent {
    fn from(value: msc_4388::QrCodeIntent) -> Self {
        match value {
            msc_4388::QrCodeIntent::Login => Self::Login,
            msc_4388::QrCodeIntent::Reciprocate => Self::Reciprocate,
        }
    }
}

impl From<QrCodeIntent> for msc_4388::QrCodeIntent {
    fn from(value: QrCodeIntent) -> Self {
        match value {
            QrCodeIntent::Login => msc_4388::QrCodeIntent::Login,
            QrCodeIntent::Reciprocate => msc_4388::QrCodeIntent::Reciprocate,
        }
    }
}

/// Data for the QR code login mechanism.
///
/// The [`QrCodeData`] can be serialized and encoded as a QR code or it can be
/// decoded from a QR code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrCodeData {
    inner: QrCodeDataInner,
}

impl QrCodeData {
    /// Create a new [`QrCodeData`] object which conforms to the data format
    /// specified in [MSC4108].
    ///
    /// [MSC4108]: https://github.com/matrix-org/matrix-spec-proposals/pull/4108
    pub fn new_msc4108(
        public_key: Curve25519PublicKey,
        rendezvous_url: Url,
        intent_data: Msc4108IntentData,
    ) -> Self {
        Self {
            inner: QrCodeDataInner::Msc4108(msc_4108::QrCodeData {
                public_key,
                rendezvous_url,
                intent_data,
            }),
        }
    }

    /// Create a new [`QrCodeData`] object which conforms to the data format
    /// specified in [MSC4388].
    ///
    /// [MSC4388]: https://github.com/matrix-org/matrix-spec-proposals/pull/4388
    pub fn new_msc4388(
        public_key: Curve25519PublicKey,
        rendezvous_id: String,
        base_url: Url,
        intent: QrCodeIntent,
    ) -> Self {
        Self {
            inner: QrCodeDataInner::Msc4388(msc_4388::QrCodeData {
                intent: intent.into(),
                public_key,
                rendezvous_id,
                base_url,
            }),
        }
    }

    /// Attempt to decode a slice of bytes into a [`QrCodeData`] object.
    ///
    /// The slice of bytes would generally be returned by a QR code decoder.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, LoginQrCodeDecodeError> {
        let inner = if bytes.starts_with(msc_4108::PREFIX) {
            msc_4108::QrCodeData::from_bytes(bytes).map(QrCodeDataInner::Msc4108)?
        } else {
            msc_4388::QrCodeData::from_bytes(bytes).map(QrCodeDataInner::Msc4388)?
        };

        Ok(QrCodeData { inner })
    }

    /// Encode the [`QrCodeData`] into a list of bytes.
    ///
    /// The list of bytes can be used by a QR code generator to create an image
    /// containing a QR code.
    pub fn to_bytes(&self) -> Vec<u8> {
        match &self.inner {
            QrCodeDataInner::Msc4108(qr_code_data) => qr_code_data.to_bytes(),
            QrCodeDataInner::Msc4388(qr_code_data) => qr_code_data.to_bytes(),
        }
    }

    /// Attempt to decode a base64 encoded string into a [`QrCodeData`] object.
    pub fn from_base64(data: &str) -> Result<Self, LoginQrCodeDecodeError> {
        let bytes = base64_decode(data)?;
        Self::from_bytes(&bytes)
    }

    /// Encode the [`QrCodeData`] into a base64 encoded string.
    pub fn to_base64(&self) -> String {
        let bytes = self.to_bytes();
        base64_encode(bytes)
    }

    /// The ephemeral Curve25519 public key. Can be used to establish a shared
    /// secret using the Diffie-Hellman key agreement.
    pub fn public_key(&self) -> Curve25519PublicKey {
        match &self.inner {
            QrCodeDataInner::Msc4108(qr_code_data) => qr_code_data.public_key,
            QrCodeDataInner::Msc4388(qr_code_data) => qr_code_data.public_key,
        }
    }

    /// Get the [`QrCodeIntent`] of this [`QrCodeData`] object.
    ///
    /// This tells us if the creator of the QR code wants to log in or if they
    /// want to log another device in.
    pub fn intent(&self) -> QrCodeIntent {
        match &self.inner {
            QrCodeDataInner::Msc4108(qr_code_data) => qr_code_data.intent().into(),
            QrCodeDataInner::Msc4388(qr_code_data) => qr_code_data.intent.clone().into(),
        }
    }

    /// The intent-specific data for the QR code.
    pub fn intent_data(&self) -> QrCodeIntentData<'_> {
        match &self.inner {
            QrCodeDataInner::Msc4108(qr_code_data) => QrCodeIntentData::Msc4108 {
                data: &qr_code_data.intent_data,
                rendezvous_url: &qr_code_data.rendezvous_url,
            },
            QrCodeDataInner::Msc4388(qr_code_data) => QrCodeIntentData::Msc4388 {
                rendezvous_id: &qr_code_data.rendezvous_id,
                base_url: &qr_code_data.base_url,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum QrCodeDataInner {
    Msc4108(msc_4108::QrCodeData),
    Msc4388(msc_4388::QrCodeData),
}
