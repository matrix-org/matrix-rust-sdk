// Copyright 2020 The Matrix.org Foundation C.I.C.
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

pub use base64::DecodeError;
use base64::{decode_config, encode_config, STANDARD_NO_PAD};

/// Decode the input as base64 with no padding.
pub fn decode(input: impl AsRef<[u8]>) -> Result<Vec<u8>, DecodeError> {
    decode_config(input, STANDARD_NO_PAD)
}

/// Encode the input as base64 with no padding.
pub fn encode(input: impl AsRef<[u8]>) -> String {
    encode_config(input, STANDARD_NO_PAD)
}

#[cfg(test)]
pub(crate) fn json_convert<T, U>(value: &T) -> serde_json::Result<U>
where
    T: serde::Serialize,
    U: serde::de::DeserializeOwned,
{
    let json = serde_json::to_string(value)?;
    serde_json::from_str(&json)
}
