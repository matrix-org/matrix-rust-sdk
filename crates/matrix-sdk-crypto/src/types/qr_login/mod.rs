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

//! Data types for the QR code login mechanism described in [MSC4108]
//!
//! [MSC4108]: https://github.com/matrix-org/matrix-spec-proposals/pull/4108

use std::str::Utf8Error;

use thiserror::Error;

mod msc_4108;

/// The version of the QR code data, currently only one version is specified.
const VERSION: u8 = 0x02;

pub use msc_4108::{QrCodeMode, QrCodeModeData};
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
    /// The QR code data contains an invalid mode, we expect the login (0x03)
    /// mode or the reciprocate mode (0x04).
    #[error(
        "The QR code data contains an invalid QR code login mode, expected 0x03 or 0x04, got {0}"
    )]
    InvalidMode(u8),
    /// The QR code data contains an unsupported version.
    #[error("The QR code data contains an unsupported version, expected {VERSION}, got {0}")]
    InvalidVersion(u8),
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
        got: [u8; 6],
    },
}

/// Data for the QR code login mechanism.
///
/// The [`QrCodeData`] can be serialized and encoded as a QR code or it can be
/// decoded from a QR code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrCodeData(QrCodeDataInner);

impl QrCodeData {
    /// Create a new [`QrCodeData`] object which conforms to the data format
    /// specified in [MSC4108].
    ///
    /// [MSC4108]: https://github.com/matrix-org/matrix-spec-proposals/pull/4108
    pub fn new_msc4108(
        public_key: Curve25519PublicKey,
        rendezvous_url: Url,
        mode_data: QrCodeModeData,
    ) -> Self {
        Self(QrCodeDataInner::Msc4108(msc_4108::QrCodeData {
            public_key,
            rendezvous_url,
            mode_data,
        }))
    }

    /// Attempt to decode a slice of bytes into a [`QrCodeData`] object.
    ///
    /// The slice of bytes would generally be returned by a QR code decoder.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, LoginQrCodeDecodeError> {
        let data = msc_4108::QrCodeData::from_bytes(bytes).map(QrCodeDataInner::Msc4108)?;

        Ok(QrCodeData(data))
    }

    /// Encode the [`QrCodeData`] into a list of bytes.
    ///
    /// The list of bytes can be used by a QR code generator to create an image
    /// containing a QR code.
    pub fn to_bytes(&self) -> Vec<u8> {
        match &self.0 {
            QrCodeDataInner::Msc4108(qr_code_data) => qr_code_data.to_bytes(),
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
        match &self.0 {
            QrCodeDataInner::Msc4108(qr_code_data) => qr_code_data.public_key,
        }
    }

    /// The URL of the rendezvous session, can be used to exchange messages with
    /// the other device.
    pub fn rendezvous_url(&self) -> &Url {
        match &self.0 {
            QrCodeDataInner::Msc4108(qr_code_data) => &qr_code_data.rendezvous_url,
        }
    }

    /// Get the [`QrCodeMode`] of this [`QrCodeData`] object.
    ///
    /// This tells us if the creator of the QR code wants to log in or if they
    /// want to log another device in.
    pub fn mode(&self) -> QrCodeMode {
        match &self.0 {
            QrCodeDataInner::Msc4108(qr_code_data) => qr_code_data.mode(),
        }
    }

    /// The mode-specific data for the QR code.
    pub fn mode_data(&self) -> &QrCodeModeData {
        match &self.0 {
            QrCodeDataInner::Msc4108(qr_code_data) => &qr_code_data.mode_data,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum QrCodeDataInner {
    Msc4108(msc_4108::QrCodeData),
}
