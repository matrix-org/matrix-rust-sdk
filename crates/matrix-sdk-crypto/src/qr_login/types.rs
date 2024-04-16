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

use std::{
    io::{Cursor, Read},
    string::FromUtf8Error,
};

use byteorder::{BigEndian, ReadBytesExt};
use thiserror::Error;
use url::Url;
use vodozemac::{base64_decode, base64_encode, Curve25519PublicKey};

const VERSION: u8 = 0x02;
const PREFIX: &[u8] = b"MATRIX";

#[derive(Debug, Error)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Error), uniffi(flat_error))]
pub enum QrCodeDecodeError {
    #[error("The QR code data is missing some fields.")]
    NotEnoughData(#[from] std::io::Error),
    #[error("One of the URLs in the QR code data is not a valid UTF-8 string")]
    NotUtf8(#[from] FromUtf8Error),
    #[error("One of the URLs in the QR code data could not be decoded: {0:?}")]
    UrlParse(#[from] url::ParseError),
    #[error(
        "The QR code data contains an invalid QR code login mode, expected 0x03 or 0x04, got {0}"
    )]
    InvalidMode(u8),
    #[error("The QR code data contains an unsupported version, expected {VERSION}, got {0}")]
    InvalidVersion(u8),
    #[error("The QR code data could not have been decoded from a base64 string: {0:?}")]
    Base64(#[from] vodozemac::Base64DecodeError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QrCodeModeData {
    Login,
    Reciprocate { homeserver_url: Url },
}

impl QrCodeModeData {
    pub fn mode_identifier(&self) -> QrCodeMode {
        match self {
            QrCodeModeData::Login => QrCodeMode::Login,
            QrCodeModeData::Reciprocate { .. } => QrCodeMode::Reciprocate,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QrCodeMode {
    Login = 0x03,
    Reciprocate = 0x04,
}

impl TryFrom<u8> for QrCodeMode {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x03 => Ok(Self::Login),
            0x04 => Ok(Self::Reciprocate),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrCodeData {
    pub public_key: Curve25519PublicKey,
    pub rendezvous_url: Url,
    pub mode: QrCodeModeData,
}

impl QrCodeData {
    pub fn from_base64(data: &str) -> Result<Self, QrCodeDecodeError> {
        Self::from_bytes(&base64_decode(data)?)
    }

    pub fn to_base64(&self) -> String {
        base64_encode(self.to_bytes())
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, QrCodeDecodeError> {
        let mut reader = Cursor::new(bytes);

        let mut prefix = [0u8; PREFIX.len()];
        let mut public_key = [0u8; Curve25519PublicKey::LENGTH];

        reader.read_exact(&mut prefix)?;
        let version = reader.read_u8()?;

        if version == VERSION {
            let mode = reader.read_u8()?;
            reader.read_exact(&mut public_key)?;

            let rendezvous_url_len = reader.read_u16::<BigEndian>()?;
            let mut rendezvous_url = vec![0u8; rendezvous_url_len.into()];

            reader.read_exact(&mut rendezvous_url)?;

            let mode =
                QrCodeMode::try_from(mode).map_err(|_| QrCodeDecodeError::InvalidMode(mode))?;

            let mode = match mode {
                QrCodeMode::Login => QrCodeModeData::Login,
                QrCodeMode::Reciprocate => {
                    let homeserver_url_len = reader.read_u16::<BigEndian>()?;
                    let mut homeserver_url = vec![0u8; homeserver_url_len.into()];
                    reader.read_exact(&mut homeserver_url)?;
                    let homeserver_url = String::from_utf8(homeserver_url)?;

                    let homeserver_url = Url::parse(&homeserver_url)?;

                    QrCodeModeData::Reciprocate { homeserver_url }
                }
            };

            let public_key = Curve25519PublicKey::from_bytes(public_key);
            let rendezvous_url = Url::parse(&String::from_utf8(rendezvous_url)?)?;

            Ok(Self { public_key, rendezvous_url: rendezvous_url, mode })
        } else {
            Err(QrCodeDecodeError::InvalidVersion(version))
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let rendezvous_url_len = (self.rendezvous_url.as_str().len() as u16).to_be_bytes();

        let encoded = [
            PREFIX,
            &[VERSION],
            &[self.mode.mode_identifier() as u8],
            self.public_key.as_bytes().as_slice(),
            &rendezvous_url_len,
            self.rendezvous_url.as_str().as_bytes(),
        ]
        .concat();

        if let QrCodeModeData::Reciprocate { homeserver_url } = &self.mode {
            let homeserver_url_len = (homeserver_url.as_str().len() as u16).to_be_bytes();

            [encoded.as_slice(), &homeserver_url_len, homeserver_url.as_str().as_bytes()].concat()
        } else {
            encoded
        }
    }
}

#[cfg(test)]
mod test {
    use similar_asserts::assert_eq;

    use super::*;

    const QR_CODE_DATA: &[u8] = &[
        0x4D, 0x41, 0x54, 0x52, 0x49, 0x58, 0x02, 0x03, 0xd8, 0x86, 0x68, 0x6a, 0xb2, 0x19, 0x7b,
        0x78, 0x0e, 0x30, 0x0a, 0x9d, 0x4a, 0x21, 0x47, 0x48, 0x07, 0x00, 0xd7, 0x92, 0x9f, 0x39,
        0xab, 0x31, 0xb9, 0xe5, 0x14, 0x37, 0x02, 0x48, 0xed, 0x6b, 0x00, 0x47, 0x68, 0x74, 0x74,
        0x70, 0x73, 0x3a, 0x2f, 0x2f, 0x72, 0x65, 0x6e, 0x64, 0x65, 0x7a, 0x76, 0x6f, 0x75, 0x73,
        0x2e, 0x6c, 0x61, 0x62, 0x2e, 0x65, 0x6c, 0x65, 0x6d, 0x65, 0x6e, 0x74, 0x2e, 0x64, 0x65,
        0x76, 0x2f, 0x65, 0x38, 0x64, 0x61, 0x36, 0x33, 0x35, 0x35, 0x2d, 0x35, 0x35, 0x30, 0x62,
        0x2d, 0x34, 0x61, 0x33, 0x32, 0x2d, 0x61, 0x31, 0x39, 0x33, 0x2d, 0x31, 0x36, 0x31, 0x39,
        0x64, 0x39, 0x38, 0x33, 0x30, 0x36, 0x36, 0x38,
    ];

    #[test]
    fn parse_qr_data() {
        let expected_curve_key =
            Curve25519PublicKey::from_base64("2IZoarIZe3gOMAqdSiFHSAcA15KfOasxueUUNwJI7Ws")
                .unwrap();

        let expected_rendezvous =
            Url::parse("https://rendezvous.lab.element.dev/e8da6355-550b-4a32-a193-1619d9830668")
                .unwrap();

        let data = QrCodeData::from_bytes(QR_CODE_DATA)
            .expect("We should be able to parse the QR code data");

        assert_eq!(
            expected_curve_key, data.public_key,
            "The parsed public key should match the expected one"
        );

        assert_eq!(
            expected_rendezvous, data.rendezvous_url,
            "The parsed rendezvous URL should match to the expected one",
        );

        assert_eq!(
            QrCodeModeData::Login,
            data.mode,
            "The parsed QR code mode should match to the expected one",
        );
    }

    #[test]
    fn qr_code_encoding_roundtrip() {
        let data = QrCodeData::from_bytes(QR_CODE_DATA)
            .expect("We should be able to parse the QR code data");

        let encoded = data.to_bytes();

        assert_eq!(
            QR_CODE_DATA, &encoded,
            "Decoding and re-encoding the QR code data should yield the same bytes"
        );
    }
}
