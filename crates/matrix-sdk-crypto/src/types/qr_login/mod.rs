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

pub use msc_4108::*;

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
