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
    str::{self},
};

use byteorder::{BigEndian, ReadBytesExt};
use url::Url;
use vodozemac::Curve25519PublicKey;

use super::LoginQrCodeDecodeError;

/// The version of the QR code data, currently only one version is specified.
const VERSION: u8 = 0x02;
/// The prefix that is used in the QR code data.
pub(super) const PREFIX: &[u8] = b"MATRIX";

/// The intent-specific data for the QR code.
///
/// The QR code login mechanism supports both, the new device, as well as the
/// existing device to display the QR code.
///
/// Depending on which device is displaying the QR code, additional data will be
/// attached to the QR code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msc4108IntentData {
    /// Enum variant for the case where the new device is displaying the QR
    /// code.
    Login,
    /// Enum variant for the case where the existing device is displaying the QR
    /// code.
    Reciprocate {
        /// The homeserver the existing device is using. This will let the new
        /// device know which homeserver it should use as well.
        server_name: String,
    },
}

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
    Login = 0x03,
    /// Enum variant for the case where the existing device is displaying the QR
    /// code.
    Reciprocate = 0x04,
}

impl TryFrom<u8> for QrCodeIntent {
    type Error = LoginQrCodeDecodeError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x03 => Ok(Self::Login),
            0x04 => Ok(Self::Reciprocate),
            intent => Err(LoginQrCodeDecodeError::InvalidIntent {
                expected_login: QrCodeIntent::Login as u8,
                expected_reciprocate: QrCodeIntent::Reciprocate as u8,
                got: intent,
            }),
        }
    }
}

impl From<&Msc4108IntentData> for QrCodeIntent {
    fn from(value: &Msc4108IntentData) -> Self {
        match value {
            Msc4108IntentData::Login => Self::Login,
            Msc4108IntentData::Reciprocate { .. } => Self::Reciprocate,
        }
    }
}

/// Data for the QR code login mechanism.
///
/// The [`QrCodeData`] can be serialized and encoded as a QR code or it can be
/// decoded from a QR code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct QrCodeData {
    /// The ephemeral Curve25519 public key. Can be used to establish a shared
    /// secret using the Diffie-Hellman key agreement.
    pub public_key: Curve25519PublicKey,
    /// The URL of the rendezvous session, can be used to exchange messages with
    /// the other device.
    pub rendezvous_url: Url,
    /// Intent specific data, may contain the homeserver URL.
    pub intent_data: Msc4108IntentData,
}

impl QrCodeData {
    /// Attempt to decode a slice of bytes into a [`QrCodeData`] object.
    ///
    /// The slice of bytes would generally be returned by a QR code decoder.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, LoginQrCodeDecodeError> {
        // The QR data consists of the following values:
        // 1. The ASCII string MATRIX.
        // 2. One byte version, only 0x02 is supported.
        // 3. One byte intent, either 0x03 or 0x04.
        // 4. 32 bytes for the ephemeral Curve25519 key.
        // 5. Two bytes for the length of the rendezvous URL, a u16 in big-endian
        //    encoding.
        // 6. The UTF-8 encoded string containing the rendezvous URL.
        // 7. If the intent from point 3. is 0x04, then two bytes for the length of the
        //    homeserver URL, a u16 in big-endian encoding.
        // 8. If the intent from point 3. is 0x04, then the UTF-8 encoded string
        //    containing the homeserver URL.
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
        let version = reader.read_u8()?;
        if version == VERSION {
            // 3. The intent is the next one to parse, we return an error immediately the
            //    intent isn't 0x03 or 0x04.
            let intent = QrCodeIntent::try_from(reader.read_u8()?)?;

            // 4. Let's get the public key and convert it to our strongly typed
            // Curve25519PublicKey type.
            let mut public_key = [0u8; Curve25519PublicKey::LENGTH];
            reader.read_exact(&mut public_key)?;
            let public_key = Curve25519PublicKey::from_bytes(public_key);

            // 5. We read two bytes for the length of the rendezvous URL.
            let rendezvous_url_len = reader.read_u16::<BigEndian>()?;
            // 6. We read and parse the rendezvous URL itself.
            let mut rendezvous_url = vec![0u8; rendezvous_url_len.into()];
            reader.read_exact(&mut rendezvous_url)?;
            let rendezvous_url = Url::parse(str::from_utf8(&rendezvous_url)?)?;

            let intent_data = match intent {
                QrCodeIntent::Login => Msc4108IntentData::Login,
                QrCodeIntent::Reciprocate => {
                    // 7. If the intent is 0x04, we attempt to read the two bytes for the length of
                    //    the homeserver URL.
                    let server_name_len = reader.read_u16::<BigEndian>()?;

                    // 8. We read and parse the homeserver URL.
                    let mut server_name = vec![0u8; server_name_len.into()];
                    reader.read_exact(&mut server_name)?;
                    let server_name = String::from_utf8(server_name).map_err(|e| e.utf8_error())?;

                    Msc4108IntentData::Reciprocate { server_name }
                }
            };

            Ok(Self { public_key, rendezvous_url, intent_data })
        } else {
            Err(LoginQrCodeDecodeError::InvalidType { expected: VERSION, got: version })
        }
    }

    /// Encode the [`QrCodeData`] into a list of bytes.
    ///
    /// The list of bytes can be used by a QR code generator to create an image
    /// containing a QR code.
    pub fn to_bytes(&self) -> Vec<u8> {
        let rendezvous_url_len = (self.rendezvous_url.as_str().len() as u16).to_be_bytes();

        let encoded = [
            PREFIX,
            &[VERSION],
            &[self.intent() as u8],
            self.public_key.as_bytes().as_slice(),
            &rendezvous_url_len,
            self.rendezvous_url.as_str().as_bytes(),
        ]
        .concat();

        if let Msc4108IntentData::Reciprocate { server_name } = &self.intent_data {
            let server_name_len = (server_name.as_str().len() as u16).to_be_bytes();

            [encoded.as_slice(), &server_name_len, server_name.as_str().as_bytes()].concat()
        } else {
            encoded
        }
    }

    /// Get the intent of this [`QrCodeData`] instance.
    pub fn intent(&self) -> QrCodeIntent {
        (&self.intent_data).into()
    }
}

#[cfg(test)]
pub(super) mod test {
    use assert_matches2::assert_let;
    use similar_asserts::assert_eq;

    use super::*;
    use crate::types::qr_login::QrCodeIntentData;

    // Test vector for the QR code data, copied from the MSC.
    pub const QR_CODE_DATA: &[u8] = &[
        0x4D, 0x41, 0x54, 0x52, 0x49, 0x58, 0x02, 0x03, 0xd8, 0x86, 0x68, 0x6a, 0xb2, 0x19, 0x7b,
        0x78, 0x0e, 0x30, 0x0a, 0x9d, 0x4a, 0x21, 0x47, 0x48, 0x07, 0x00, 0xd7, 0x92, 0x9f, 0x39,
        0xab, 0x31, 0xb9, 0xe5, 0x14, 0x37, 0x02, 0x48, 0xed, 0x6b, 0x00, 0x47, 0x68, 0x74, 0x74,
        0x70, 0x73, 0x3a, 0x2f, 0x2f, 0x72, 0x65, 0x6e, 0x64, 0x65, 0x7a, 0x76, 0x6f, 0x75, 0x73,
        0x2e, 0x6c, 0x61, 0x62, 0x2e, 0x65, 0x6c, 0x65, 0x6d, 0x65, 0x6e, 0x74, 0x2e, 0x64, 0x65,
        0x76, 0x2f, 0x65, 0x38, 0x64, 0x61, 0x36, 0x33, 0x35, 0x35, 0x2d, 0x35, 0x35, 0x30, 0x62,
        0x2d, 0x34, 0x61, 0x33, 0x32, 0x2d, 0x61, 0x31, 0x39, 0x33, 0x2d, 0x31, 0x36, 0x31, 0x39,
        0x64, 0x39, 0x38, 0x33, 0x30, 0x36, 0x36, 0x38,
    ];

    // Test vector for the QR code data, copied from the MSC, with the intent set to
    // reciprocate.
    const QR_CODE_DATA_RECIPROCATE: &[u8] = &[
        0x4D, 0x41, 0x54, 0x52, 0x49, 0x58, 0x02, 0x04, 0xd8, 0x86, 0x68, 0x6a, 0xb2, 0x19, 0x7b,
        0x78, 0x0e, 0x30, 0x0a, 0x9d, 0x4a, 0x21, 0x47, 0x48, 0x07, 0x00, 0xd7, 0x92, 0x9f, 0x39,
        0xab, 0x31, 0xb9, 0xe5, 0x14, 0x37, 0x02, 0x48, 0xed, 0x6b, 0x00, 0x47, 0x68, 0x74, 0x74,
        0x70, 0x73, 0x3a, 0x2f, 0x2f, 0x72, 0x65, 0x6e, 0x64, 0x65, 0x7a, 0x76, 0x6f, 0x75, 0x73,
        0x2e, 0x6c, 0x61, 0x62, 0x2e, 0x65, 0x6c, 0x65, 0x6d, 0x65, 0x6e, 0x74, 0x2e, 0x64, 0x65,
        0x76, 0x2f, 0x65, 0x38, 0x64, 0x61, 0x36, 0x33, 0x35, 0x35, 0x2d, 0x35, 0x35, 0x30, 0x62,
        0x2d, 0x34, 0x61, 0x33, 0x32, 0x2d, 0x61, 0x31, 0x39, 0x33, 0x2d, 0x31, 0x36, 0x31, 0x39,
        0x64, 0x39, 0x38, 0x33, 0x30, 0x36, 0x36, 0x38, 0x00, 0x0A, 0x6d, 0x61, 0x74, 0x72, 0x69,
        0x78, 0x2e, 0x6f, 0x72, 0x67,
    ];

    // Test vector for the QR code data in base64 format, self-generated.
    pub const QR_CODE_DATA_BASE64: &str = "\
        TUFUUklYAgS0yzZ1QVpQ1jlnoxWX3d5jrWRFfELxjS2gN7pz9y+3PABaaHR0\
        cHM6Ly9zeW5hcHNlLW9pZGMubGFiLmVsZW1lbnQuZGV2L19zeW5hcHNlL2Ns\
        aWVudC9yZW5kZXp2b3VzLzAxSFg5SzAwUTFINktQRDQ3RUc0RzFUM1hHACVo\
        dHRwczovL3N5bmFwc2Utb2lkYy5sYWIuZWxlbWVudC5kZXYv";

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
            "The parsed rendezvous URL should match expected one",
        );

        assert_eq!(
            data.intent(),
            QrCodeIntent::Login,
            "The intent in the test bytes vector should be Login"
        );

        assert_eq!(
            Msc4108IntentData::Login,
            data.intent_data,
            "The parsed QR code intent should match expected one",
        );
    }

    #[test]
    fn parse_qr_data_reciprocate() {
        let expected_curve_key =
            Curve25519PublicKey::from_base64("2IZoarIZe3gOMAqdSiFHSAcA15KfOasxueUUNwJI7Ws")
                .unwrap();

        let expected_rendezvous =
            Url::parse("https://rendezvous.lab.element.dev/e8da6355-550b-4a32-a193-1619d9830668")
                .unwrap();

        let data = QrCodeData::from_bytes(QR_CODE_DATA_RECIPROCATE)
            .expect("We should be able to parse the QR code data");

        assert_eq!(
            expected_curve_key, data.public_key,
            "The parsed public key should match the expected one"
        );

        assert_eq!(
            expected_rendezvous, data.rendezvous_url,
            "The parsed rendezvous URL should match expected one",
        );

        assert_eq!(
            data.intent(),
            QrCodeIntent::Reciprocate,
            "The intent in the test bytes vector should be Reciprocate"
        );

        assert_let!(
            Msc4108IntentData::Reciprocate { server_name } = data.intent_data,
            "The parsed QR code intent should match the expected one",
        );

        assert_eq!(
            server_name, "matrix.org",
            "We should have correctly found the matrix.org homeserver in the QR code data"
        );
    }

    #[test]
    fn parse_qr_data_base64() {
        let expected_curve_key =
            Curve25519PublicKey::from_base64("tMs2dUFaUNY5Z6MVl93eY61kRXxC8Y0toDe6c/cvtzw")
                .unwrap();

        let expected_rendezvous =
            Url::parse("https://synapse-oidc.lab.element.dev/_synapse/client/rendezvous/01HX9K00Q1H6KPD47EG4G1T3XG")
                .unwrap();

        let expected_server_name = "https://synapse-oidc.lab.element.dev/";

        let data = crate::types::qr_login::QrCodeData::from_base64(QR_CODE_DATA_BASE64)
            .expect("We should be able to parse the QR code data");

        assert_eq!(
            expected_curve_key,
            data.public_key(),
            "The parsed public key should match the expected one"
        );

        assert_eq!(
            data.intent(),
            QrCodeIntent::Reciprocate.into(),
            "The intent in the test bytes vector should be Reciprocate"
        );

        assert_let!(
            QrCodeIntentData::Msc4108 {
                data: Msc4108IntentData::Reciprocate { server_name },
                rendezvous_url
            } = data.intent_data()
        );

        assert_eq!(
            &expected_rendezvous, rendezvous_url,
            "The parsed rendezvous URL should match the expected one",
        );

        assert_eq!(
            server_name, expected_server_name,
            "The parsed server name should match the expected one"
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

        let data = crate::types::qr_login::QrCodeData::from_base64(QR_CODE_DATA_BASE64)
            .expect("We should be able to parse the QR code data");

        let encoded = data.to_base64();

        assert_eq!(
            QR_CODE_DATA_BASE64, &encoded,
            "Decoding and re-encoding the QR code data should yield the same base64 string"
        );
    }
}
