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

#[derive(Error, Debug)]
pub enum DecodingError {
    #[cfg(feature = "decode_image")]
    #[cfg_attr(feature = "docs", doc(cfg(decode_image)))]
    #[error(transparent)]
    Qr(#[from] rqrr::DeQRError),
    #[error("the decoded QR code is missing the Matrix header")]
    Header,
    #[error(transparent)]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("the QR code contains an invalid verification mode: {0}")]
    Mode(u8),
    #[error(transparent)]
    Identifier(#[from] ruma_identifiers::Error),
    #[error(transparent)]
    Read(#[from] std::io::Error),
    #[error("the QR code contains a too short shared secret, length: {0}")]
    SharedSecret(usize),
    #[error("the QR code contains an invalid or unsupported version: {0}")]
    Version(u8),
}

#[derive(Error, Debug)]
pub enum EncodingError {
    #[error(transparent)]
    Qr(#[from] qrcode::types::QrError),
    #[error(transparent)]
    Base64(#[from] base64::DecodeError),
    #[error("The verification flow id length can't be converted into a u16: {0}")]
    FlowId(#[from] std::num::TryFromIntError),
}
