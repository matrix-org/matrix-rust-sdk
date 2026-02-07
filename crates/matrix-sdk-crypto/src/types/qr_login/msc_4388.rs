// Copyright 2026 The Matrix.org Foundation C.I.C.
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

//! Data types for the QR code login mechanism described in [MSC4388]
//!
//! [MSC4388]: https://github.com/matrix-org/matrix-spec-proposals/pull/4388

use std::{
    io::{Cursor, Read},
    str::{self},
};

use byteorder::{BigEndian, ReadBytesExt};
use url::Url;
use vodozemac::Curve25519PublicKey;

use super::LoginQrCodeDecodeError;

/// The unstable prefix that is used in the QR code data.
const PREFIX: &[u8] = b"IO_ELEMENT_MSC4388";
/// The type of the QR code data, used to identify MSC4388.
const TYPE: u8 = 0x03;

/// The intent of the device that generated/displayed the QR code.
///
/// The QR code login mechanism supports both, the new device, as well as the
/// existing device to display the QR code.
///
/// The different intents have an explicit one-byte identifier which gets added
/// to the QR code data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QrCodeIntent {
    /// Enum variant for the case where the new device is displaying the QR
    /// code.
    Login = 0x00,
    /// Enum variant for the case where the existing device is displaying the QR
    /// code.
    Reciprocate = 0x01,
}

impl TryFrom<u8> for QrCodeIntent {
    type Error = LoginQrCodeDecodeError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::Login),
            0x01 => Ok(Self::Reciprocate),
            intent => Err(LoginQrCodeDecodeError::InvalidIntent {
                expected_login: QrCodeIntent::Login as u8,
                expected_reciprocate: QrCodeIntent::Reciprocate as u8,
                got: intent,
            }),
        }
    }
}

/// Data for the QR code login mechanism.
///
/// The [`QrCodeData`] can be serialized and encoded as a QR code or it can be
/// decoded from a QR code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrCodeData {
    /// The intent of the QR code.
    pub intent: QrCodeIntent,
    /// The ephemeral Curve25519 public key. Can be used to establish a shared
    /// secret using the Diffie-Hellman key agreement.
    pub public_key: Curve25519PublicKey,
    /// The ID of the rendezvous session, can be used to exchange messages with
    /// the other device.
    pub rendezvous_id: String,
    /// The base URL of the homeserver that the device generating the QR is
    /// using.
    pub base_url: Url,
}

impl QrCodeData {
    /// Attempt to decode a slice of bytes into a [`QrCodeData`] object.
    ///
    /// The slice of bytes would generally be returned by a QR code decoder.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, LoginQrCodeDecodeError> {
        // The QR data consists of the following values:
        // 1. The unstable ASCII string IO_ELEMENT_MSC4388.
        // 2. One byte type, 0x03 is for the format specified in MSC4388.
        // 3. One byte intent, either 0x00 or 0x01.
        // 4. 32 bytes for the ephemeral Curve25519 key.
        // 5. Two bytes for the length of the rendezvous ID, a u16 in big-endian
        //    encoding.
        // 6. The UTF-8 encoded string containing the rendezvous ID.
        // 7. Two bytes for the length of the server base URL, a u16 in big-endian
        //    encoding.
        // 8. The UTF-8 encoded string containing the server base URL.
        let mut reader = Cursor::new(bytes);

        // 1. Let's get the prefix first and double check if this QR code is intended
        //    for the QR code login mechanism.
        let mut prefix = [0u8; PREFIX.len()];
        reader.read_exact(&mut prefix)?;

        if PREFIX != prefix {
            return Err(LoginQrCodeDecodeError::InvalidPrefix {
                expected: PREFIX,
                got: prefix.to_vec(),
            });
        }

        // 2. Next up is the version, we continue only if the version matches.
        let qr_type = reader.read_u8()?;

        if qr_type == TYPE {
            // 3. The intent is the next one to parse, we return an error immediately if the
            //    intent isn't 0x00 or 0x01.
            let intent = QrCodeIntent::try_from(reader.read_u8()?)?;

            // 4. Let's get the public key and convert it to our strongly typed
            // Curve25519PublicKey type.
            let mut public_key = [0u8; Curve25519PublicKey::LENGTH];
            reader.read_exact(&mut public_key)?;
            let public_key = Curve25519PublicKey::from_bytes(public_key);

            // 5. We read two bytes for the length of the rendezvous ID.
            let rendezvous_id_len = reader.read_u16::<BigEndian>()?;
            // 6. We read the rendezvous ID itself.
            let mut rendezvous_id = vec![0u8; rendezvous_id_len.into()];
            reader.read_exact(&mut rendezvous_id)?;
            let rendezvous_id = String::from_utf8(rendezvous_id).map_err(|e| e.utf8_error())?;

            // 7. We read the two bytes for the length of the server base URL.
            let base_url_len = reader.read_u16::<BigEndian>()?;

            // 8. We read and parse the server base URL.
            let mut base_url = vec![0u8; base_url_len.into()];
            reader.read_exact(&mut base_url)?;
            let base_url = Url::parse(str::from_utf8(&base_url)?)?;

            Ok(Self { public_key, rendezvous_id, base_url, intent })
        } else {
            Err(LoginQrCodeDecodeError::InvalidType { expected: TYPE, got: qr_type })
        }
    }

    /// Encode the [`QrCodeData`] into a list of bytes.
    ///
    /// The list of bytes can be used by a QR code generator to create an image
    /// containing a QR code.
    pub fn to_bytes(&self) -> Vec<u8> {
        let rendezvous_id_len = (self.rendezvous_id.as_str().len() as u16).to_be_bytes();

        // if path is / then don't include the trailing slash
        let base_url = if self.base_url.path() == "/" {
            self.base_url.as_str().trim_end_matches('/')
        } else {
            self.base_url.as_str()
        };
        let base_url_len = (base_url.len() as u16).to_be_bytes();

        [
            PREFIX,
            &[TYPE],
            &[self.intent.clone() as u8],
            self.public_key.as_bytes().as_slice(),
            &rendezvous_id_len,
            self.rendezvous_id.as_bytes(),
            &base_url_len,
            base_url.as_bytes(),
        ]
        .concat()
    }
}

#[cfg(test)]
mod test {
    use assert_matches2::assert_let;
    use similar_asserts::assert_eq;

    use super::*;
    use crate::types::qr_login::QrCodeDataInner;

    // Test vector for the unstable QR code data, copied from the MSC.
    const QR_CODE_DATA_RECIPROCATE: &[u8] = &[
        0x49, 0x4F, 0x5F, 0x45, 0x4C, 0x45, 0x4D, 0x45, 0x4E, 0x54, 0x5F, 0x4D, 0x53, 0x43, 0x34,
        0x33, 0x38, 0x38, 0x03, 0x01, 0xd8, 0x86, 0x68, 0x6a, 0xb2, 0x19, 0x7b, 0x78, 0x0e, 0x30,
        0x0a, 0x9d, 0x4a, 0x21, 0x47, 0x48, 0x07, 0x00, 0xd7, 0x92, 0x9f, 0x39, 0xab, 0x31, 0xb9,
        0xe5, 0x14, 0x37, 0x02, 0x48, 0xed, 0x6b, 0x00, 0x24, 0x65, 0x38, 0x64, 0x61, 0x36, 0x33,
        0x35, 0x35, 0x2D, 0x35, 0x35, 0x30, 0x62, 0x2D, 0x34, 0x61, 0x33, 0x32, 0x2D, 0x61, 0x31,
        0x39, 0x33, 0x2D, 0x31, 0x36, 0x31, 0x39, 0x64, 0x39, 0x38, 0x33, 0x30, 0x36, 0x36, 0x38,
        0x00, 0x20, 0x68, 0x74, 0x74, 0x70, 0x73, 0x3A, 0x2F, 0x2F, 0x6D, 0x61, 0x74, 0x72, 0x69,
        0x78, 0x2D, 0x63, 0x6C, 0x69, 0x65, 0x6E, 0x74, 0x2E, 0x6D, 0x61, 0x74, 0x72, 0x69, 0x78,
        0x2E, 0x6F, 0x72, 0x67,
    ];

    // Test vector for the QR code data, copied from the MSC, with the intent set to
    // login
    const QR_CODE_DATA_LOGIN: &[u8] = &[
        0x49, 0x4F, 0x5F, 0x45, 0x4C, 0x45, 0x4D, 0x45, 0x4E, 0x54, 0x5F, 0x4D, 0x53, 0x43, 0x34,
        0x33, 0x38, 0x38, 0x03, 0x00, 0xd8, 0x86, 0x68, 0x6a, 0xb2, 0x19, 0x7b, 0x78, 0x0e, 0x30,
        0x0a, 0x9d, 0x4a, 0x21, 0x47, 0x48, 0x07, 0x00, 0xd7, 0x92, 0x9f, 0x39, 0xab, 0x31, 0xb9,
        0xe5, 0x14, 0x37, 0x02, 0x48, 0xed, 0x6b, 0x00, 0x24, 0x65, 0x38, 0x64, 0x61, 0x36, 0x33,
        0x35, 0x35, 0x2D, 0x35, 0x35, 0x30, 0x62, 0x2D, 0x34, 0x61, 0x33, 0x32, 0x2D, 0x61, 0x31,
        0x39, 0x33, 0x2D, 0x31, 0x36, 0x31, 0x39, 0x64, 0x39, 0x38, 0x33, 0x30, 0x36, 0x36, 0x38,
        0x00, 0x20, 0x68, 0x74, 0x74, 0x70, 0x73, 0x3A, 0x2F, 0x2F, 0x6D, 0x61, 0x74, 0x72, 0x69,
        0x78, 0x2D, 0x63, 0x6C, 0x69, 0x65, 0x6E, 0x74, 0x2E, 0x6D, 0x61, 0x74, 0x72, 0x69, 0x78,
        0x2E, 0x6F, 0x72, 0x67,
    ];

    // Test vector for the QR code data in base64 format, self-generated.
    const QR_CODE_DATA_BASE64: &str = "SU9fRUxFTUVOVF9NU0M0Mzg4AwG0yzZ1QVpQ1jlnoxWX3d5jrWRFfELxjS2gN7pz9y+3PAAaMDFIWDlLMDBRMUg2S1BENDdFRzRHMVQzWEcAJGh0dHBzOi8vc3luYXBzZS1vaWRjLmxhYi5lbGVtZW50LmRldg";

    #[test]
    fn parse_qr_data() {
        let expected_curve_key =
            Curve25519PublicKey::from_base64("2IZoarIZe3gOMAqdSiFHSAcA15KfOasxueUUNwJI7Ws")
                .unwrap();

        let data = QrCodeData::from_bytes(QR_CODE_DATA_LOGIN)
            .expect("We should be able to parse the QR code data");

        assert_eq!(
            QrCodeIntent::Login,
            data.intent,
            "The intent in the test bytes vector should be Login"
        );

        assert_eq!(
            expected_curve_key, data.public_key,
            "The parsed public key should match the expected one"
        );

        assert_eq!(
            "e8da6355-550b-4a32-a193-1619d9830668", data.rendezvous_id,
            "The parsed rendezvous ID should match expected one",
        );

        assert_eq!(
            "https://matrix-client.matrix.org/",
            data.base_url.as_str(),
            "We should have correctly found the matrix.org server name in the QR code data"
        );
    }

    #[test]
    fn parse_qr_data_reciprocate() {
        let expected_curve_key =
            Curve25519PublicKey::from_base64("2IZoarIZe3gOMAqdSiFHSAcA15KfOasxueUUNwJI7Ws")
                .unwrap();

        let data = QrCodeData::from_bytes(QR_CODE_DATA_RECIPROCATE)
            .expect("We should be able to parse the QR code data");

        assert_eq!(
            expected_curve_key, data.public_key,
            "The parsed public key should match the expected one"
        );

        assert_eq!(
            "e8da6355-550b-4a32-a193-1619d9830668", data.rendezvous_id,
            "The parsed rendezvous URL should match expected one",
        );

        assert_eq!(
            QrCodeIntent::Reciprocate,
            data.intent,
            "The intent in the test bytes vector should be Reciprocate"
        );

        assert_eq!(
            data.base_url.as_str(),
            "https://matrix-client.matrix.org/",
            "We should have correctly found the matrix.org homeserver in the QR code data"
        );
    }

    #[test]
    fn parse_qr_data_base64() {
        let expected_curve_key =
            Curve25519PublicKey::from_base64("tMs2dUFaUNY5Z6MVl93eY61kRXxC8Y0toDe6c/cvtzw")
                .unwrap();

        let data = crate::types::qr_login::QrCodeData::from_base64(QR_CODE_DATA_BASE64)
            .expect("We should be able to parse the QR code data");

        assert_let!(
            crate::types::qr_login::QrCodeData { inner: QrCodeDataInner::Msc4388(data) } = data
        );

        assert_eq!(
            expected_curve_key, data.public_key,
            "The parsed public key should match the expected one"
        );

        assert_eq!(
            QrCodeIntent::Reciprocate,
            data.intent,
            "The intent in the test bytes vector should be Reciprocate"
        );

        assert_eq!(
            "01HX9K00Q1H6KPD47EG4G1T3XG", data.rendezvous_id,
            "The parsed rendezvous URL should match the expected one",
        );

        assert_eq!(
            "https://synapse-oidc.lab.element.dev/",
            data.base_url.as_str(),
            "The parsed server name should match the expected one"
        );
    }

    #[test]
    fn qr_code_encoding_roundtrip() {
        let data = QrCodeData::from_bytes(QR_CODE_DATA_LOGIN)
            .expect("We should be able to parse the QR code data");

        let encoded = data.to_bytes();

        assert_eq!(
            QR_CODE_DATA_LOGIN, &encoded,
            "Decoding and re-encoding the QR code data should yield the same bytes"
        );

        let data = crate::types::qr_login::QrCodeData::from_base64(QR_CODE_DATA_BASE64)
            .expect("We should be able to parse the QR code data");

        let encoded = data.to_base64();

        assert_eq!(
            QR_CODE_DATA_BASE64, &encoded,
            "Decoding and re-encoding the QR code data should yield the same base64 string"
        );
    }
}
