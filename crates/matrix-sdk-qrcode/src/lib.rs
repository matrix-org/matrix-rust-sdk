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

#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]
#![warn(missing_debug_implementations, missing_docs)]

mod error;
mod types;
mod utils;

pub use error::{DecodingError, EncodingError};
#[cfg(feature = "decode_image")]
pub use image;
pub use qrcode;
#[cfg(feature = "decode_image")]
pub use rqrr;
pub use types::{
    QrVerificationData, SelfVerificationData, SelfVerificationNoMasterKey, VerificationData,
};

#[cfg(test)]
mod tests {
    #[cfg(feature = "decode_image")]
    use std::io::Cursor;

    #[cfg(feature = "decode_image")]
    use image::{ImageFormat, Luma};
    #[cfg(feature = "decode_image")]
    use qrcode::QrCode;

    #[cfg(feature = "decode_image")]
    use crate::utils::decode_qr;
    use crate::{DecodingError, QrVerificationData};

    #[cfg(feature = "decode_image")]
    static VERIFICATION: &[u8; 4277] = include_bytes!("../data/verification.png");
    #[cfg(feature = "decode_image")]
    static SELF_VERIFICATION: &[u8; 1467] = include_bytes!("../data/self-verification.png");
    #[cfg(feature = "decode_image")]
    static SELF_NO_MASTER: &[u8; 1775] = include_bytes!("../data/self-no-master.png");

    #[test]
    #[cfg(feature = "decode_image")]
    fn decode_qr_test() {
        let image = Cursor::new(VERIFICATION);
        let image = image::load(image, ImageFormat::Png).unwrap().to_luma8();
        decode_qr(image).expect("Couldn't decode the QR code");
    }

    #[test]
    #[cfg(feature = "decode_image")]
    fn decode_test() {
        let image = Cursor::new(VERIFICATION);
        let image = image::load(image, ImageFormat::Png).unwrap().to_luma8();
        let result = QrVerificationData::try_from(image).unwrap();

        assert!(matches!(result, QrVerificationData::Verification(_)));
    }

    #[test]
    #[cfg(feature = "decode_image")]
    fn decode_encode_cycle() {
        let image = Cursor::new(VERIFICATION);
        let image = image::load(image, ImageFormat::Png).unwrap();
        let result = QrVerificationData::from_image(image).unwrap();

        assert!(matches!(result, QrVerificationData::Verification(_)));

        let encoded = result.to_qr_code().unwrap();
        let image = encoded.render::<Luma<u8>>().build();
        let second_result = QrVerificationData::try_from(image).unwrap();

        assert_eq!(result, second_result);

        let bytes = result.to_bytes().unwrap();
        let third_result = QrVerificationData::from_bytes(bytes).unwrap();

        assert_eq!(result, third_result);
    }

    #[test]
    #[cfg(feature = "decode_image")]
    fn decode_encode_cycle_self() {
        let image = Cursor::new(SELF_VERIFICATION);
        let image = image::load(image, ImageFormat::Png).unwrap();
        let result = QrVerificationData::try_from(image).unwrap();

        assert!(matches!(result, QrVerificationData::SelfVerification(_)));

        let encoded = result.to_qr_code().unwrap();
        let image = encoded.render::<Luma<u8>>().build();
        let second_result = QrVerificationData::from_luma(image).unwrap();

        assert_eq!(result, second_result);

        let bytes = result.to_bytes().unwrap();
        let third_result = QrVerificationData::from_bytes(bytes).unwrap();

        assert_eq!(result, third_result);
    }

    #[test]
    #[cfg(feature = "decode_image")]
    fn decode_encode_cycle_self_no_master() {
        let image = Cursor::new(SELF_NO_MASTER);
        let image = image::load(image, ImageFormat::Png).unwrap();
        let result = QrVerificationData::from_image(image).unwrap();

        assert!(matches!(result, QrVerificationData::SelfVerificationNoMasterKey(_)));

        let encoded = result.to_qr_code().unwrap();
        let image = encoded.render::<Luma<u8>>().build();
        let second_result = QrVerificationData::try_from(image).unwrap();

        assert_eq!(result, second_result);

        let bytes = result.to_bytes().unwrap();
        let third_result = QrVerificationData::try_from(bytes).unwrap();

        assert_eq!(result, third_result);
    }

    #[test]
    #[cfg(feature = "decode_image")]
    fn decode_invalid_qr() {
        let qr = QrCode::new(b"NonMatrixCode").expect("Can't build a simple QR code");
        let image = qr.render::<Luma<u8>>().build();
        let result = QrVerificationData::try_from(image);
        assert!(matches!(result, Err(DecodingError::Header)))
    }

    #[test]
    fn decode_invalid_header() {
        let data = b"NonMatrixCode";
        let result = QrVerificationData::from_bytes(data);
        assert!(matches!(result, Err(DecodingError::Header)))
    }

    #[test]
    fn decode_invalid_mode() {
        let data = b"MATRIX\x02\x03";
        let result = QrVerificationData::from_bytes(data);
        assert!(matches!(result, Err(DecodingError::Mode(3))))
    }

    #[test]
    fn decode_invalid_version() {
        let data = b"MATRIX\x01\x03";
        let result = QrVerificationData::from_bytes(data);
        assert!(matches!(result, Err(DecodingError::Version(1))))
    }

    #[test]
    fn decode_missing_data() {
        let data = b"MATRIX\x02\x02";
        let result = QrVerificationData::from_bytes(data);
        assert!(matches!(result, Err(DecodingError::Read(_))))
    }

    #[test]
    fn decode_short_secret() {
        let data = b"MATRIX\
                   \x02\x02\x00\x07\
                   FLOW_ID\
                   AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\
                   BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\
                   SECRET";

        let result = QrVerificationData::from_bytes(data);
        assert!(matches!(result, Err(DecodingError::SharedSecret(_))))
    }

    #[test]
    fn decode_invalid_room_id() {
        let data = b"MATRIX\
                   \x02\x00\x00\x0f\
                   test:localhost\
                   kS /\x92i\x1e6\xcd'g\xf9#\x11\xd8\x8a\xa2\xf61\x05\x1b6\xef\xfc\xa4%\x80\x1a\x0c\xd2\xe8\x04\
                   \xbdR|\xf8n\x07\xa4\x1f\xb4\xcc3\x0eBT\xe7[~\xfd\x87\xd06B\xdfoVv%\x9b\x86\xae\xbcM\
                   SECRETISLONGENOUGH";

        let result = QrVerificationData::from_bytes(data);
        assert!(matches!(result, Err(DecodingError::Identifier(_))))
    }

    #[test]
    fn decode_invalid_keys() {
        let data = b"MATRIX\
                   \x02\x00\x00\x0f\
                   !test:localhost\
                   AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\
                   BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\
                   SECRETISLONGENOUGH";
        let result = QrVerificationData::from_bytes(data);
        assert!(matches!(result, Err(DecodingError::Keys(_))))
    }
}
