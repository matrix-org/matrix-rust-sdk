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

use std::io::{Cursor, Read, Seek, SeekFrom};

use base64::{decode_config, encode_config, DecodeError, STANDARD_NO_PAD};
use byteorder::{BigEndian, ReadBytesExt};

use aes_ctr::{
    stream_cipher::{NewStreamCipher, SyncStreamCipher},
    Aes128Ctr,
};
use hmac::{Hmac, Mac, NewMac};
use pbkdf2::pbkdf2;
use sha2::{Sha256, Sha512};

const SALT_SIZE: usize = 16;
const IV_SIZE: usize = 16;
const MAC_SIZE: usize = 32;
const KEY_SIZE: usize = 16;

pub fn decode(input: impl AsRef<[u8]>) -> Result<Vec<u8>, DecodeError> {
    decode_config(input, STANDARD_NO_PAD)
}

pub fn encode(input: impl AsRef<[u8]>) -> String {
    encode_config(input, STANDARD_NO_PAD)
}

pub fn decrypt(ciphertext: &str, passphrase: String) -> Result<String, DecodeError> {
    let decoded = decode(ciphertext)?;

    let mut decoded = Cursor::new(decoded);

    let mut salt = [0u8; SALT_SIZE];
    let mut iv = [0u8; IV_SIZE];
    let mut mac = [0u8; MAC_SIZE];
    let mut derived_keys = [0u8; KEY_SIZE * 2];

    let version = decoded.read_u8().unwrap();

    decoded.read_exact(&mut salt).unwrap();
    decoded.read_exact(&mut iv).unwrap();

    let rounds = decoded.read_u32::<BigEndian>().unwrap();
    let ciphertext_start = decoded.position() as usize;

    decoded.seek(SeekFrom::End(-32)).unwrap();
    let ciphertext_end = decoded.position() as usize;

    decoded.read_exact(&mut mac).unwrap();

    let mut decoded = decoded.into_inner();
    let ciphertext = &decoded[0..ciphertext_end];

    if version != 1u8 {
        panic!("Unsupported version")
    }

    pbkdf2::<Hmac<Sha512>>(passphrase.as_bytes(), &salt, rounds, &mut derived_keys);
    let (key, hmac_key) = derived_keys.split_at(KEY_SIZE / 2);

    let mut hmac = Hmac::<Sha256>::new_varkey(hmac_key).unwrap();
    hmac.update(ciphertext);
    hmac.verify(&mac).expect("MAC DOESN'T MATCH");

    let mut ciphertext = &mut decoded[ciphertext_start..ciphertext_end];

    let mut aes = Aes128Ctr::new_var(&key, &iv).expect("Can't create AES");

    aes.apply_keystream(&mut ciphertext);

    Ok(String::from_utf8(ciphertext.to_owned()).expect("Invalid utf-8"))
}
