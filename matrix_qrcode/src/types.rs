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

use std::{
    convert::TryFrom,
    io::{Cursor, Read},
};

use byteorder::{BigEndian, ReadBytesExt};
#[cfg(feature = "decode_image")]
use image::{DynamicImage, ImageBuffer, Luma};
use qrcode::QrCode;
use ruma_identifiers::EventId;

#[cfg(feature = "decode_image")]
#[cfg_attr(feature = "docs", doc(cfg(decode_image)))]
use crate::utils::decode_qr;
use crate::{
    error::{DecodingError, EncodingError},
    utils::{base_64_encode, to_bytes, to_qr_code, HEADER, MAX_MODE, MIN_SECRET_LEN, VERSION},
};

#[derive(Clone, Debug, PartialEq)]
pub enum QrVerification {
    Verification(VerificationData),
    SelfVerification(SelfVerificationData),
    SelfVerificationNoMasterKey(SelfVerificationNoMasterKey),
}

#[cfg(feature = "decode_image")]
#[cfg_attr(feature = "docs", doc(cfg(decode_image)))]
impl TryFrom<DynamicImage> for QrVerification {
    type Error = DecodingError;

    fn try_from(image: DynamicImage) -> Result<Self, Self::Error> {
        Self::from_image(image)
    }
}

#[cfg(feature = "decode_image")]
#[cfg_attr(feature = "docs", doc(cfg(decode_image)))]
impl TryFrom<ImageBuffer<Luma<u8>, Vec<u8>>> for QrVerification {
    type Error = DecodingError;

    fn try_from(image: ImageBuffer<Luma<u8>, Vec<u8>>) -> Result<Self, Self::Error> {
        Self::from_luma(image)
    }
}

impl TryFrom<Vec<u8>> for QrVerification {
    type Error = DecodingError;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        Self::from_bytes(value)
    }
}

impl QrVerification {
    #[cfg(feature = "decode_image")]
    #[cfg_attr(feature = "docs", doc(cfg(decode_image)))]
    pub fn from_image(image: DynamicImage) -> Result<Self, DecodingError> {
        let image = image.to_luma8();
        Self::decode(image)
    }

    #[cfg(feature = "decode_image")]
    #[cfg_attr(feature = "docs", doc(cfg(decode_image)))]
    pub fn from_luma(image: ImageBuffer<Luma<u8>, Vec<u8>>) -> Result<Self, DecodingError> {
        Self::decode(image)
    }

    pub fn from_bytes(bytes: impl AsRef<[u8]>) -> Result<Self, DecodingError> {
        Self::decode_bytes(bytes)
    }

    pub fn to_qr_code(&self) -> Result<QrCode, EncodingError> {
        match self {
            QrVerification::Verification(v) => v.to_qr_code(),
            QrVerification::SelfVerification(v) => v.to_qr_code(),
            QrVerification::SelfVerificationNoMasterKey(v) => v.to_qr_code(),
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, EncodingError> {
        match self {
            QrVerification::Verification(v) => v.to_bytes(),
            QrVerification::SelfVerification(v) => v.to_bytes(),
            QrVerification::SelfVerificationNoMasterKey(v) => v.to_bytes(),
        }
    }

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

        QrVerification::new(mode, flow_id, first_key, second_key, shared_secret)
    }

    #[cfg(feature = "decode_image")]
    #[cfg_attr(feature = "docs", doc(cfg(decode_image)))]
    fn decode(image: ImageBuffer<Luma<u8>, Vec<u8>>) -> Result<QrVerification, DecodingError> {
        let decoded = decode_qr(image)?;
        Self::decode_bytes(decoded)
    }

    fn new(
        mode: u8,
        flow_id: Vec<u8>,
        first_key: [u8; 32],
        second_key: [u8; 32],
        shared_secret: Vec<u8>,
    ) -> Result<Self, DecodingError> {
        let first_key = base_64_encode(&first_key);
        let second_key = base_64_encode(&second_key);
        let flow_id = String::from_utf8(flow_id)?;
        let shared_secret = base_64_encode(&shared_secret);

        match mode {
            VerificationData::QR_MODE => {
                let event_id = EventId::try_from(flow_id)?;
                Ok(VerificationData::new(event_id, first_key, second_key, shared_secret).into())
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
}

#[derive(Clone, Debug, PartialEq)]
pub struct VerificationData {
    event_id: EventId,
    first_master_key: String,
    second_master_key: String,
    shared_secret: String,
}

impl VerificationData {
    const QR_MODE: u8 = 0x00;

    pub fn to_bytes(&self) -> Result<Vec<u8>, EncodingError> {
        to_bytes(
            Self::QR_MODE,
            &self.event_id.as_str(),
            &self.first_master_key,
            &self.second_master_key,
            &self.shared_secret,
        )
    }

    pub fn to_qr_code(&self) -> Result<QrCode, EncodingError> {
        to_qr_code(
            Self::QR_MODE,
            self.event_id.as_str(),
            &self.first_master_key,
            &self.second_master_key,
            &self.shared_secret,
        )
    }

    pub fn new(
        event_id: EventId,
        first_key: String,
        second_key: String,
        shared_secret: String,
    ) -> Self {
        Self { event_id, first_master_key: first_key, second_master_key: second_key, shared_secret }
    }
}

impl From<VerificationData> for QrVerification {
    fn from(data: VerificationData) -> Self {
        Self::Verification(data)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SelfVerificationData {
    transaction_id: String,
    master_key: String,
    device_key: String,
    shared_secret: String,
}

impl SelfVerificationData {
    const QR_MODE: u8 = 0x01;

    pub fn to_bytes(&self) -> Result<Vec<u8>, EncodingError> {
        to_bytes(
            Self::QR_MODE,
            &self.transaction_id,
            &self.master_key,
            &self.device_key,
            &self.shared_secret,
        )
    }

    pub fn to_qr_code(&self) -> Result<QrCode, EncodingError> {
        to_qr_code(
            Self::QR_MODE,
            &self.transaction_id,
            &self.master_key,
            &self.device_key,
            &self.shared_secret,
        )
    }

    pub fn new(
        transaction_id: String,
        master_key: String,
        device_key: String,
        shared_secret: String,
    ) -> Self {
        Self { transaction_id, master_key, device_key, shared_secret }
    }
}

impl From<SelfVerificationData> for QrVerification {
    fn from(data: SelfVerificationData) -> Self {
        Self::SelfVerification(data)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SelfVerificationNoMasterKey {
    transaction_id: String,
    device_key: String,
    master_key: String,
    shared_secret: String,
}

impl SelfVerificationNoMasterKey {
    const QR_MODE: u8 = 0x02;

    pub fn to_bytes(&self) -> Result<Vec<u8>, EncodingError> {
        to_bytes(
            Self::QR_MODE,
            &self.transaction_id,
            &self.device_key,
            &self.master_key,
            &self.shared_secret,
        )
    }

    pub fn to_qr_code(&self) -> Result<QrCode, EncodingError> {
        to_qr_code(
            Self::QR_MODE,
            &self.transaction_id,
            &self.device_key,
            &self.master_key,
            &self.shared_secret,
        )
    }

    pub fn new(
        transaction_id: String,
        device_key: String,
        master_key: String,
        shared_secret: String,
    ) -> Self {
        Self { transaction_id, device_key, master_key, shared_secret }
    }
}

impl From<SelfVerificationNoMasterKey> for QrVerification {
    fn from(data: SelfVerificationNoMasterKey) -> Self {
        Self::SelfVerificationNoMasterKey(data)
    }
}
