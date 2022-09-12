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
pub use qrcode;
pub use types::{
    QrVerificationData, SelfVerificationData, SelfVerificationNoMasterKey, VerificationData,
};

#[cfg(test)]
mod tests {
    use crate::{DecodingError, QrVerificationData};

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
