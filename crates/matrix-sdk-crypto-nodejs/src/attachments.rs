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

use matrix_sdk_crypto::{AttachmentDecryptor, AttachmentEncryptor, MediaEncryptionInfo};
use napi_derive::napi;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Serialize, Deserialize)]
pub struct EncryptedMedia {
    pub info: MediaEncryptionInfo,
    pub data: Vec<u8>,
}

#[napi]
pub fn encrypt_file(data: String) -> String {
    let data: Vec<u8> = serde_json::from_str(data.as_str()).expect("Failed to decode input array");
    let mut cursor = Cursor::new(data);
    let mut encryptor = AttachmentEncryptor::new(&mut cursor);
    let mut encrypted = Vec::<u8>::new();
    encryptor.read_to_end(&mut encrypted).unwrap();
    let info = encryptor.finish();
    serde_json::to_string(&json!({
        "data": encrypted,
        "info": info,
    }))
    .expect("Failed to serialize json")
}

#[napi]
pub fn decrypt_file(payload: String) -> String {
    let payload: (Vec<u8>, MediaEncryptionInfo) =
        serde_json::from_str(payload.as_str()).expect("Failed to decode inputs");
    let data = payload.0;
    let info = payload.1;
    let mut cursor = Cursor::new(data);
    let mut decryptor = AttachmentDecryptor::new(&mut cursor, info).unwrap();
    let mut decrypted = Vec::<u8>::new();
    decryptor.read_to_end(&mut decrypted).unwrap();
    serde_json::to_string(&json!({
        "data": decrypted,
    }))
    .expect("Failed to serialize json")
}
