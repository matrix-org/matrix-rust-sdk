// Copyright 2021 The Matrix.org Foundation C.I.C.
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

use thiserror::Error;

/// Error type describing errors that happen while QR data is being decoded.
#[derive(Error, Debug)]
pub enum DecodingError {
    /// The QR code data is missing the mandatory Matrix header.
    #[error("the decoded QR code is missing the Matrix header")]
    Header,
    /// The QR code data is containing an invalid, non UTF-8, flow id.
    #[error(transparent)]
    Utf8(#[from] std::string::FromUtf8Error),
    /// The QR code data is using an unsupported or invalid verification mode.
    #[error("the QR code contains an invalid verification mode: {0}")]
    Mode(u8),
    #[error(transparent)]
    /// The QR code data does not contain all the necessary fields.
    Read(#[from] std::io::Error),
    /// The QR code data uses an invalid shared secret.
    #[error("the QR code contains a too short shared secret, length: {0}")]
    SharedSecret(usize),
    /// The QR code data uses an invalid or unsupported version.
    #[error("the QR code contains an invalid or unsupported version: {0}")]
    Version(u8),
    /// The QR code data doesn't contain valid ed25519 keys.
    #[error("the QR code contains invalid ed25519 keys: {0}")]
    Keys(#[from] vodozemac::KeyError),
}

/// Error type describing errors that happen while QR data is being encoded.
#[derive(Error, Debug)]
pub enum EncodingError {
    /// Error generating a QR code from the data, likely because the data
    /// doesn't fit into a QR code.
    #[error(transparent)]
    Qr(#[from] qrcode::types::QrError),
    /// Error encoding the given flow id, the flow id is too large.
    #[error("The verification flow id length can't be converted into a u16: {0}")]
    FlowId(#[from] std::num::TryFromIntError),
}
