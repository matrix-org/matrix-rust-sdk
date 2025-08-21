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

use std::io::{Cursor, Read};

use byteorder::{BigEndian, ReadBytesExt};
use qrcode::QrCode;
use ruma::serde::Base64;
use vodozemac::Ed25519PublicKey;

use crate::{
    error::{DecodingError, EncodingError},
    utils::{to_bytes, to_qr_code, HEADER, MAX_MODE, MIN_SECRET_LEN, VERSION},
};

/// An enum representing the different modes for a QR verification code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QrVerificationData {
    /// The QR verification is verifying another user.
    ///
    /// It relies on both devices already trusting or owning the master
    /// cross-signing key for the corresponding user.
    ///
    /// In this case, the QR code data includes:
    ///  * The master cross-signing key of the displaying device's user.
    ///  * What the displaying device believes is the master cross-signing key
    ///    of the scanning device's user.
    ///
    /// After a successful verification, each device will trust the
    /// cross-signing key of the other user, and will upload a cross-signature
    /// of that key.
    Verification(VerificationData),

    /// The QR verification is self-verifying and the device displaying the QR
    /// code trusts or owns the master cross-signing key.
    ///
    /// This normally happens when the displaying device is an existing device,
    /// and the scanning device is new.
    ///
    /// In this case, the QR code data includes:
    ///  * The master cross-signing key (which is trusted by the displaying
    ///    device).
    ///  * What the displaying device believes is the device key of the scanning
    ///    device.
    ///
    /// After a successful verification, the scanning device will be able to
    /// trust the master key, and the displaying device will be able to
    /// trust the scanning device's device key.
    ///
    /// Since the displaying device should be cross-signed already, this means
    /// that the scanning device will now trust the displaying device.
    ///
    /// The displaying device will then upload a cross-signature of the scanning
    /// device (assuming it has the private key), and will send the secret keys
    /// to the scanning device.
    SelfVerification(SelfVerificationData),

    /// The QR verification is self-verifying in which the current device does
    /// not yet trust the master key.
    ///
    /// This normally happens when the displaying device is new, and the
    /// scanning device is an existing device.
    ///
    /// In this case, the QR code data includes:
    ///  * The displaying device's device key.
    ///  * What the displaying device believes is the master cross-signing key.
    ///
    /// If the verification is successful, the scanning device will be able to
    /// trust the displaying device's device key, and the displaying device will
    /// be able to trust the master key.
    ///
    /// Since the scanning device should be cross-signed already, this means
    /// that the displaying device will now trust the scanning device.
    ///
    /// The scanning device will then upload a cross-signature of the displaying
    /// device (assuming it has the private key), and will send the secret keys
    /// to the displaying device.
    SelfVerificationNoMasterKey(SelfVerificationNoMasterKey),
}

impl TryFrom<&[u8]> for QrVerificationData {
    type Error = DecodingError;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        Self::from_bytes(value)
    }
}

impl TryFrom<Vec<u8>> for QrVerificationData {
    type Error = DecodingError;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        Self::from_bytes(value)
    }
}

impl QrVerificationData {
    /// Parse the decoded payload of a QR code in byte slice form as a
    /// `QrVerificationData`
    ///
    /// This method is useful if you would like to do your own custom QR code
    /// decoding.
    ///
    /// # Arguments
    ///
    /// * `bytes` - The raw bytes of a decoded QR code.
    ///
    /// # Examples
    /// ```
    /// # use matrix_sdk_qrcode::{QrVerificationData, DecodingError};
    /// # fn main() -> Result<(), DecodingError> {
    /// let data = b"MATRIX\
    ///              \x02\x02\x00\x07\
    ///              FLOW_ID\
    ///              kS /\x92i\x1e6\xcd'g\xf9#\x11\xd8\x8a\xa2\xf61\x05\x1b6\xef\xfc\xa4%\x80\x1a\x0c\xd2\xe8\x04\
    ///              \xbdR|\xf8n\x07\xa4\x1f\xb4\xcc3\x0eBT\xe7[~\xfd\x87\xd06B\xdfoVv%\x9b\x86\xae\xbcM\
    ///              SHARED_SECRET";
    ///
    /// let result = QrVerificationData::from_bytes(data)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_bytes(bytes: impl AsRef<[u8]>) -> Result<Self, DecodingError> {
        Self::decode_bytes(bytes)
    }

    /// Encode the `QrVerificationData` into a `QrCode`.
    ///
    /// This method turns the `QrVerificationData` into a QR code that can be
    /// rendered and presented to be scanned.
    ///
    /// The encoding can fail if the data doesn't fit into a QR code or if the
    /// identity keys that should be encoded into the QR code are not valid
    /// base64.
    ///
    /// # Examples
    /// ```
    /// # use matrix_sdk_qrcode::{QrVerificationData, DecodingError};
    /// # fn main() -> Result<(), DecodingError> {
    /// let data = b"MATRIX\
    ///              \x02\x02\x00\x07\
    ///              FLOW_ID\
    ///              kS /\x92i\x1e6\xcd'g\xf9#\x11\xd8\x8a\xa2\xf61\x05\x1b6\xef\xfc\xa4%\x80\x1a\x0c\xd2\xe8\x04\
    ///              \xbdR|\xf8n\x07\xa4\x1f\xb4\xcc3\x0eBT\xe7[~\xfd\x87\xd06B\xdfoVv%\x9b\x86\xae\xbcM\
    ///              SHARED_SECRET";
    ///
    /// let result = QrVerificationData::from_bytes(data)?;
    /// let encoded = result.to_qr_code().unwrap();
    /// # Ok(())
    /// # }
    /// ```
    pub fn to_qr_code(&self) -> Result<QrCode, EncodingError> {
        match self {
            QrVerificationData::Verification(v) => v.to_qr_code(),
            QrVerificationData::SelfVerification(v) => v.to_qr_code(),
            QrVerificationData::SelfVerificationNoMasterKey(v) => v.to_qr_code(),
        }
    }

    /// Encode the `QrVerificationData` into a vector of bytes that can be
    /// encoded as a QR code.
    ///
    /// The encoding can fail if the identity keys that should be encoded are
    /// not valid base64.
    ///
    /// # Examples
    /// ```
    /// # use matrix_sdk_qrcode::{QrVerificationData, DecodingError};
    /// # fn main() -> Result<(), DecodingError> {
    /// let data = b"MATRIX\
    ///              \x02\x02\x00\x07\
    ///              FLOW_ID\
    ///              kS /\x92i\x1e6\xcd'g\xf9#\x11\xd8\x8a\xa2\xf61\x05\x1b6\xef\xfc\xa4%\x80\x1a\x0c\xd2\xe8\x04\
    ///              \xbdR|\xf8n\x07\xa4\x1f\xb4\xcc3\x0eBT\xe7[~\xfd\x87\xd06B\xdfoVv%\x9b\x86\xae\xbcM\
    ///              SHARED_SECRET";
    ///
    /// let result = QrVerificationData::from_bytes(data)?;
    /// let encoded = result.to_bytes().unwrap();
    ///
    /// assert_eq!(data.as_ref(), encoded.as_slice());
    /// # Ok(())
    /// # }
    /// ```
    pub fn to_bytes(&self) -> Result<Vec<u8>, EncodingError> {
        match self {
            QrVerificationData::Verification(v) => v.to_bytes(),
            QrVerificationData::SelfVerification(v) => v.to_bytes(),
            QrVerificationData::SelfVerificationNoMasterKey(v) => v.to_bytes(),
        }
    }

    /// Decode the byte slice containing the decoded QR code data.
    ///
    /// The format is defined in the [spec].
    ///
    /// The byte slice consists of the following parts:
    ///
    /// * the ASCII string MATRIX
    /// * one byte indicating the QR code version (must be 0x02)
    /// * one byte indicating the QR code verification mode. one of the
    ///   following values:
    ///     * 0x00 verifying another user with cross-signing
    ///     * 0x01 self-verifying in which the current device does trust the
    ///       master key
    ///     * 0x02 self-verifying in which the current device does not yet trust
    ///       the master key
    /// * the event ID or transaction_id of the associated verification request
    ///   event, encoded as:
    ///     * two bytes in network byte order (big-endian) indicating the length
    ///       in bytes of the ID as a UTF-8 string
    ///     * the ID as a UTF-8 string
    /// * the first key, as 32 bytes
    /// * the second key, as 32 bytes
    /// * a random shared secret, as a byte string. as we do not share the
    ///   length of the secret, and it is not a fixed size, clients will just
    ///   use the remainder of binary string as the shared secret.
    ///
    /// [spec]: https://spec.matrix.org/unstable/client-server-api/#qr-code-format
    fn decode_bytes(bytes: impl AsRef<[u8]>) -> Result<Self, DecodingError> {
        let mut decoded = Cursor::new(bytes);

        let mut header = [0u8; 6];
        let mut first_key = [0u8; 32];
        let mut second_key = [0u8; 32];

        decoded.read_exact(&mut header)?;
        let version = decoded.read_u8()?;
        let mode = decoded.read_u8()?;

        if header != HEADER {
            return Err(DecodingError::Header);
        } else if version != VERSION {
            return Err(DecodingError::Version(version));
        } else if mode > MAX_MODE {
            return Err(DecodingError::Mode(mode));
        }

        let flow_id_len = decoded.read_u16::<BigEndian>()?;
        let mut flow_id = vec![0; flow_id_len.into()];

        decoded.read_exact(&mut flow_id)?;
        decoded.read_exact(&mut first_key)?;
        decoded.read_exact(&mut second_key)?;

        let mut shared_secret = Vec::new();

        decoded.read_to_end(&mut shared_secret)?;

        if shared_secret.len() < MIN_SECRET_LEN {
            return Err(DecodingError::SharedSecret(shared_secret.len()));
        }

        let first_key = Ed25519PublicKey::from_slice(&first_key)?;
        let second_key = Ed25519PublicKey::from_slice(&second_key)?;

        QrVerificationData::new(mode, flow_id, first_key, second_key, shared_secret)
    }

    fn new(
        mode: u8,
        flow_id: Vec<u8>,
        first_key: Ed25519PublicKey,
        second_key: Ed25519PublicKey,
        shared_secret: Vec<u8>,
    ) -> Result<Self, DecodingError> {
        let flow_id = String::from_utf8(flow_id)?;
        let shared_secret = Base64::new(shared_secret);

        match mode {
            VerificationData::QR_MODE => {
                Ok(VerificationData::new(flow_id, first_key, second_key, shared_secret).into())
            }
            SelfVerificationData::QR_MODE => {
                Ok(SelfVerificationData::new(flow_id, first_key, second_key, shared_secret).into())
            }
            SelfVerificationNoMasterKey::QR_MODE => {
                Ok(SelfVerificationNoMasterKey::new(flow_id, first_key, second_key, shared_secret)
                    .into())
            }
            m => Err(DecodingError::Mode(m)),
        }
    }

    /// Get the flow id for this `QrVerificationData`.
    ///
    /// This represents the ID as a string even if it is a `EventId`.
    pub fn flow_id(&self) -> &str {
        match self {
            QrVerificationData::Verification(v) => v.flow_id.as_str(),
            QrVerificationData::SelfVerification(v) => &v.transaction_id,
            QrVerificationData::SelfVerificationNoMasterKey(v) => &v.transaction_id,
        }
    }

    /// Get the first key of this `QrVerificationData`.
    pub fn first_key(&self) -> Ed25519PublicKey {
        match self {
            QrVerificationData::Verification(v) => v.first_master_key,
            QrVerificationData::SelfVerification(v) => v.master_key,
            QrVerificationData::SelfVerificationNoMasterKey(v) => v.device_key,
        }
    }

    /// Get the second key of this `QrVerificationData`.
    pub fn second_key(&self) -> Ed25519PublicKey {
        match self {
            QrVerificationData::Verification(v) => v.second_master_key,
            QrVerificationData::SelfVerification(v) => v.device_key,
            QrVerificationData::SelfVerificationNoMasterKey(v) => v.master_key,
        }
    }

    /// Get the secret of this `QrVerificationData`.
    pub fn secret(&self) -> &Base64 {
        match self {
            QrVerificationData::Verification(v) => &v.shared_secret,
            QrVerificationData::SelfVerification(v) => &v.shared_secret,
            QrVerificationData::SelfVerificationNoMasterKey(v) => &v.shared_secret,
        }
    }
}

/// The non-encoded data for the first mode of QR code verification.
///
/// This mode is used for verification between two users using their master
/// cross signing keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationData {
    flow_id: String,
    first_master_key: Ed25519PublicKey,
    second_master_key: Ed25519PublicKey,
    shared_secret: Base64,
}

impl VerificationData {
    const QR_MODE: u8 = 0x00;

    /// Create a new `VerificationData` struct that can be encoded as a QR code.
    ///
    /// # Arguments
    /// * `flow_id` - The event ID or transaction ID of the
    ///   `m.key.verification.request` event that initiated the verification
    ///   flow this QR code should be part of.
    ///
    /// * `first_master_key` - Our own cross signing master key.
    ///
    /// * `second_master_key` - The cross signing master key of the other user.
    ///
    /// * `shared_secret` - A random bytestring encoded as unpadded base64,
    ///   needs to be at least 8 bytes long.
    pub fn new(
        flow_id: String,
        first_master_key: Ed25519PublicKey,
        second_master_key: Ed25519PublicKey,
        shared_secret: Base64,
    ) -> Self {
        Self { flow_id, first_master_key, second_master_key, shared_secret }
    }

    /// Encode the `VerificationData` into a vector of bytes that can be
    /// encoded as a QR code.
    ///
    /// The encoding can fail if the master keys that should be encoded are not
    /// valid base64.
    ///
    /// # Examples
    /// ```
    /// # use matrix_sdk_qrcode::{QrVerificationData, DecodingError};
    /// # fn main() -> Result<(), DecodingError> {
    /// let data = b"MATRIX\
    ///              \x02\x00\x00\x0f\
    ///              $test:localhost\
    ///              kS /\x92i\x1e6\xcd'g\xf9#\x11\xd8\x8a\xa2\xf61\x05\x1b6\xef\xfc\xa4%\x80\x1a\x0c\xd2\xe8\x04\
    ///              \xbdR|\xf8n\x07\xa4\x1f\xb4\xcc3\x0eBT\xe7[~\xfd\x87\xd06B\xdfoVv%\x9b\x86\xae\xbcM\
    ///              SHARED_SECRET";
    ///
    /// let result = QrVerificationData::from_bytes(data)?;
    /// if let QrVerificationData::Verification(decoded) = result {
    ///     let encoded = decoded.to_bytes().unwrap();
    ///     assert_eq!(data.as_ref(), encoded.as_slice());
    /// } else {
    ///     panic!("Data was encoded as an incorrect mode");
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn to_bytes(&self) -> Result<Vec<u8>, EncodingError> {
        to_bytes(
            Self::QR_MODE,
            self.flow_id.as_str(),
            self.first_master_key,
            self.second_master_key,
            &self.shared_secret,
        )
    }

    /// Encode the `VerificationData` into a `QrCode`.
    ///
    /// This method turns the `VerificationData` into a QR code that can be
    /// rendered and presented to be scanned.
    ///
    /// The encoding can fail if the data doesn't fit into a QR code or if the
    /// keys that should be encoded into the QR code are not valid base64.
    pub fn to_qr_code(&self) -> Result<QrCode, EncodingError> {
        to_qr_code(
            Self::QR_MODE,
            self.flow_id.as_str(),
            self.first_master_key,
            self.second_master_key,
            &self.shared_secret,
        )
    }
}

impl From<VerificationData> for QrVerificationData {
    fn from(data: VerificationData) -> Self {
        Self::Verification(data)
    }
}

/// The non-encoded data for the second mode of QR code verification.
///
/// This mode is used for verification between two devices of the same user
/// where this device, that is creating this QR code, is trusting or owning
/// the cross signing master key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfVerificationData {
    transaction_id: String,
    master_key: Ed25519PublicKey,
    device_key: Ed25519PublicKey,
    shared_secret: Base64,
}

impl SelfVerificationData {
    const QR_MODE: u8 = 0x01;

    /// Create a new `SelfVerificationData` struct that can be encoded as a QR
    /// code.
    ///
    /// # Arguments
    /// * `transaction_id` - The transaction id of this verification flow, the
    ///   transaction id was sent by the `m.key.verification.request` event that
    ///   initiated the verification flow this QR code should be part of.
    ///
    /// * `master_key` - Our own cross signing master key.
    ///
    /// * `device_key` - The ed25519 key of the other device.
    ///
    /// * `shared_secret` - A random bytestring encoded as unpadded base64,
    ///   needs to be at least 8 bytes long.
    pub fn new(
        transaction_id: String,
        master_key: Ed25519PublicKey,
        device_key: Ed25519PublicKey,
        shared_secret: Base64,
    ) -> Self {
        Self { transaction_id, master_key, device_key, shared_secret }
    }

    /// Encode the `SelfVerificationData` into a vector of bytes that can be
    /// encoded as a QR code.
    ///
    /// The encoding can fail if the keys that should be encoded are not valid
    /// base64.
    ///
    /// # Examples
    /// ```
    /// # use matrix_sdk_qrcode::{QrVerificationData, DecodingError};
    /// # fn main() -> Result<(), DecodingError> {
    /// let data = b"MATRIX\
    ///              \x02\x01\x00\x06\
    ///              FLOWID\
    ///              kS /\x92i\x1e6\xcd'g\xf9#\x11\xd8\x8a\xa2\xf61\x05\x1b6\xef\xfc\xa4%\x80\x1a\x0c\xd2\xe8\x04\
    ///              \xbdR|\xf8n\x07\xa4\x1f\xb4\xcc3\x0eBT\xe7[~\xfd\x87\xd06B\xdfoVv%\x9b\x86\xae\xbcM\
    ///              SHARED_SECRET";
    ///
    /// let result = QrVerificationData::from_bytes(data)?;
    /// if let QrVerificationData::SelfVerification(decoded) = result {
    ///     let encoded = decoded.to_bytes().unwrap();
    ///     assert_eq!(data.as_ref(), encoded.as_slice());
    /// } else {
    ///     panic!("Data was encoded as an incorrect mode");
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn to_bytes(&self) -> Result<Vec<u8>, EncodingError> {
        to_bytes(
            Self::QR_MODE,
            &self.transaction_id,
            self.master_key,
            self.device_key,
            &self.shared_secret,
        )
    }

    /// Encode the `SelfVerificationData` into a `QrCode`.
    ///
    /// This method turns the `SelfVerificationData` into a QR code that can be
    /// rendered and presented to be scanned.
    ///
    /// The encoding can fail if the data doesn't fit into a QR code or if the
    /// keys that should be encoded into the QR code are not valid base64.
    pub fn to_qr_code(&self) -> Result<QrCode, EncodingError> {
        to_qr_code(
            Self::QR_MODE,
            &self.transaction_id,
            self.master_key,
            self.device_key,
            &self.shared_secret,
        )
    }
}

impl From<SelfVerificationData> for QrVerificationData {
    fn from(data: SelfVerificationData) -> Self {
        Self::SelfVerification(data)
    }
}

/// The non-encoded data for the third mode of QR code verification.
///
/// This mode is used for verification between two devices of the same user
/// where this device, that is creating this QR code, is not trusting the
/// cross signing master key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfVerificationNoMasterKey {
    transaction_id: String,
    device_key: Ed25519PublicKey,
    master_key: Ed25519PublicKey,
    shared_secret: Base64,
}

impl SelfVerificationNoMasterKey {
    const QR_MODE: u8 = 0x02;

    /// Create a new `SelfVerificationData` struct that can be encoded as a QR
    /// code.
    ///
    /// # Arguments
    /// * `transaction_id` - The transaction id of this verification flow, the
    ///   transaction id was sent by the `m.key.verification.request` event that
    ///   initiated the verification flow this QR code should be part of.
    ///
    /// * `device_key` - The ed25519 key of our own device.
    ///
    /// * `master_key` - Our own cross signing master key.
    ///
    /// * `shared_secret` - A random bytestring encoded as unpadded base64,
    ///   needs to be at least 8 bytes long.
    pub fn new(
        transaction_id: String,
        device_key: Ed25519PublicKey,
        master_key: Ed25519PublicKey,
        shared_secret: Base64,
    ) -> Self {
        Self { transaction_id, device_key, master_key, shared_secret }
    }

    /// Encode the `SelfVerificationNoMasterKey` into a vector of bytes that can
    /// be encoded as a QR code.
    ///
    /// The encoding can fail if the keys that should be encoded are not valid
    /// base64.
    ///
    /// # Examples
    /// ```
    /// # use matrix_sdk_qrcode::{QrVerificationData, DecodingError};
    /// # fn main() -> Result<(), DecodingError> {
    /// let data = b"MATRIX\
    ///              \x02\x02\x00\x06\
    ///              FLOWID\
    ///              kS /\x92i\x1e6\xcd'g\xf9#\x11\xd8\x8a\xa2\xf61\x05\x1b6\xef\xfc\xa4%\x80\x1a\x0c\xd2\xe8\x04\
    ///              \xbdR|\xf8n\x07\xa4\x1f\xb4\xcc3\x0eBT\xe7[~\xfd\x87\xd06B\xdfoVv%\x9b\x86\xae\xbcM\
    ///              SHARED_SECRET";
    ///
    /// let result = QrVerificationData::from_bytes(data)?;
    /// if let QrVerificationData::SelfVerificationNoMasterKey(decoded) = result {
    ///     let encoded = decoded.to_bytes().unwrap();
    ///     assert_eq!(data.as_ref(), encoded.as_slice());
    /// } else {
    ///     panic!("Data was encoded as an incorrect mode");
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn to_bytes(&self) -> Result<Vec<u8>, EncodingError> {
        to_bytes(
            Self::QR_MODE,
            &self.transaction_id,
            self.device_key,
            self.master_key,
            &self.shared_secret,
        )
    }

    /// Encode the `SelfVerificationNoMasterKey` into a `QrCode`.
    ///
    /// This method turns the `SelfVerificationNoMasterKey` into a QR code that
    /// can be rendered and presented to be scanned.
    ///
    /// The encoding can fail if the data doesn't fit into a QR code or if the
    /// keys that should be encoded into the QR code are not valid base64.
    pub fn to_qr_code(&self) -> Result<QrCode, EncodingError> {
        to_qr_code(
            Self::QR_MODE,
            &self.transaction_id,
            self.device_key,
            self.master_key,
            &self.shared_secret,
        )
    }
}

impl From<SelfVerificationNoMasterKey> for QrVerificationData {
    fn from(data: SelfVerificationNoMasterKey) -> Self {
        Self::SelfVerificationNoMasterKey(data)
    }
}
