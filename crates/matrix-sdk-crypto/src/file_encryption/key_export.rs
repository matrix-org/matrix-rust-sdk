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

use aes::{
    cipher::{generic_array::GenericArray, KeyIvInit, StreamCipher},
    Aes256,
};
use byteorder::{BigEndian, ReadBytesExt};
use hmac::{Hmac, Mac};
use pbkdf2::pbkdf2;
use rand::{thread_rng, RngCore};
use serde_json::Error as SerdeError;
use sha2::{Sha256, Sha512};
use thiserror::Error;
use zeroize::Zeroize;

use crate::{
    olm::ExportedRoomKey,
    utilities::{decode, encode, DecodeError},
};

type Aes256Ctr = ctr::Ctr128BE<Aes256>;

const SALT_SIZE: usize = 16;
const IV_SIZE: usize = 16;
const MAC_SIZE: usize = 32;
const KEY_SIZE: usize = 32;
const VERSION: u8 = 1;

const HEADER: &str = "-----BEGIN MEGOLM SESSION DATA-----";
const FOOTER: &str = "-----END MEGOLM SESSION DATA-----";

/// Error representing a failure during key export or import.
#[derive(Error, Debug)]
pub enum KeyExportError {
    /// The key export doesn't contain valid headers.
    #[error("Invalid or missing key export headers.")]
    InvalidHeaders,
    /// The key export has been encrypted with an unsupported version.
    #[error("The key export has been encrypted with an unsupported version.")]
    UnsupportedVersion,
    /// The MAC of the encrypted payload is invalid.
    #[error("The MAC of the encrypted payload is invalid.")]
    InvalidMac,
    /// The decrypted key export isn't valid UTF-8.
    #[error(transparent)]
    InvalidUtf8(#[from] std::string::FromUtf8Error),
    /// The decrypted key export doesn't contain valid JSON.
    #[error(transparent)]
    Json(#[from] SerdeError),
    /// The key export string isn't valid base64.
    #[error(transparent)]
    Decode(#[from] DecodeError),
    /// The key export doesn't all the required fields.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Try to decrypt a reader into a list of exported room keys.
///
/// # Arguments
///
/// * `passphrase` - The passphrase that was used to encrypt the exported keys.
///
/// # Example
///
/// ```no_run
/// # use std::io::Cursor;
/// # use matrix_sdk_crypto::{OlmMachine, decrypt_room_key_export};
/// # use ruma::{device_id, user_id};
/// # let alice = user_id!("@alice:example.org");
/// # async {
/// # let machine = OlmMachine::new(&alice, device_id!("DEVICEID")).await;
/// # let export = Cursor::new("".to_owned());
/// let exported_keys = decrypt_room_key_export(export, "1234").unwrap();
/// machine.import_room_keys(exported_keys, false, |_, _| {}).await.unwrap();
/// # };
/// ```
pub fn decrypt_room_key_export(
    mut input: impl Read,
    passphrase: &str,
) -> Result<Vec<ExportedRoomKey>, KeyExportError> {
    let mut x: String = String::new();

    input.read_to_string(&mut x)?;

    if !(x.trim_start().starts_with(HEADER) && x.trim_end().ends_with(FOOTER)) {
        return Err(KeyExportError::InvalidHeaders);
    }

    let payload: String =
        x.lines().filter(|l| !(l.starts_with(HEADER) || l.starts_with(FOOTER))).collect();

    let mut decrypted = decrypt_helper(&payload, passphrase)?;

    let ret = serde_json::from_str(&decrypted);

    decrypted.zeroize();

    Ok(ret?)
}

/// Encrypt the list of exported room keys using the given passphrase.
///
/// # Arguments
///
/// * `keys` - A list of sessions that should be encrypted.
///
/// * `passphrase` - The passphrase that will be used to encrypt the exported
/// room keys.
///
/// * `rounds` - The number of rounds that should be used for the key
/// derivation when the passphrase gets turned into an AES key. More rounds are
/// increasingly computationally intensive and as such help against brute-force
/// attacks. Should be at least `10_000`, while values in the `100_000` ranges
/// should be preferred.
///
/// # Panics
///
/// This method will panic if it can't get enough randomness from the OS to
/// encrypt the exported keys securely.
///
/// # Example
///
/// ```no_run
/// # use matrix_sdk_crypto::{OlmMachine, encrypt_room_key_export};
/// # use ruma::{device_id, user_id, room_id};
/// # let alice = user_id!("@alice:example.org");
/// # async {
/// # let machine = OlmMachine::new(&alice, device_id!("DEVICEID")).await;
/// let room_id = room_id!("!test:localhost");
/// let exported_keys = machine.export_room_keys(|s| s.room_id() == room_id).await.unwrap();
/// let encrypted_export = encrypt_room_key_export(&exported_keys, "1234", 1);
/// # };
/// ```
pub fn encrypt_room_key_export(
    keys: &[ExportedRoomKey],
    passphrase: &str,
    rounds: u32,
) -> Result<String, SerdeError> {
    let mut plaintext = serde_json::to_string(keys)?.into_bytes();
    let ciphertext = encrypt_helper(&mut plaintext, passphrase, rounds);

    plaintext.zeroize();

    Ok([HEADER.to_owned(), ciphertext, FOOTER.to_owned()].join("\n"))
}

fn encrypt_helper(plaintext: &mut [u8], passphrase: &str, rounds: u32) -> String {
    let mut salt = [0u8; SALT_SIZE];
    let mut iv = [0u8; IV_SIZE];
    let mut derived_keys = [0u8; KEY_SIZE * 2];

    let mut rng = thread_rng();

    rng.fill_bytes(&mut salt);
    rng.fill_bytes(&mut iv);

    let mut iv = u128::from_be_bytes(iv);
    iv &= !(1 << 63);
    let iv = iv.to_be_bytes();

    pbkdf2::<Hmac<Sha512>>(passphrase.as_bytes(), &salt, rounds, &mut derived_keys);
    let (key, hmac_key) = derived_keys.split_at(KEY_SIZE);

    // This is fine because the key is guaranteed to be 32 bytes, derive 64
    // bytes and split at the middle.
    let key_array = GenericArray::from_slice(key);

    let mut aes = Aes256Ctr::new(key_array, &iv.into());
    aes.apply_keystream(plaintext);

    let mut payload: Vec<u8> = vec![];

    payload.extend(VERSION.to_be_bytes());
    payload.extend(salt);
    payload.extend(iv);
    payload.extend(rounds.to_be_bytes());
    payload.extend_from_slice(plaintext);

    let mut hmac = Hmac::<Sha256>::new_from_slice(hmac_key).expect("Can't create HMAC object");
    hmac.update(&payload);
    let mac = hmac.finalize();

    payload.extend(mac.into_bytes());

    derived_keys.zeroize();

    encode(payload)
}

fn decrypt_helper(ciphertext: &str, passphrase: &str) -> Result<String, KeyExportError> {
    let decoded = decode(ciphertext)?;

    let mut decoded = Cursor::new(decoded);

    let mut salt = [0u8; SALT_SIZE];
    let mut iv = [0u8; IV_SIZE];
    let mut mac = [0u8; MAC_SIZE];
    let mut derived_keys = [0u8; KEY_SIZE * 2];

    let version = decoded.read_u8()?;
    decoded.read_exact(&mut salt)?;
    decoded.read_exact(&mut iv)?;

    let rounds = decoded.read_u32::<BigEndian>()?;
    let ciphertext_start = decoded.position() as usize;

    decoded.seek(SeekFrom::End(-32))?;
    let ciphertext_end = decoded.position() as usize;

    decoded.read_exact(&mut mac)?;

    let mut decoded = decoded.into_inner();

    if version != VERSION {
        return Err(KeyExportError::UnsupportedVersion);
    }

    pbkdf2::<Hmac<Sha512>>(passphrase.as_bytes(), &salt, rounds, &mut derived_keys);
    let (key, hmac_key) = derived_keys.split_at(KEY_SIZE);

    let mut hmac = Hmac::<Sha256>::new_from_slice(hmac_key).expect("Can't create an HMAC object");
    hmac.update(&decoded[0..ciphertext_end]);
    hmac.verify_slice(&mac).map_err(|_| KeyExportError::InvalidMac)?;

    // This is fine because the key is guaranteed to be 32 bytes, derive 64
    // bytes and split at the middle.
    let key_array = GenericArray::from_slice(key);

    let ciphertext = &mut decoded[ciphertext_start..ciphertext_end];
    let mut aes = Aes256Ctr::new(key_array, &iv.into());
    aes.apply_keystream(ciphertext);

    let ret = String::from_utf8(ciphertext.to_owned());

    derived_keys.zeroize();
    ciphertext.zeroize();

    Ok(ret?)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod proptests {
    use proptest::prelude::*;

    use super::{decrypt_helper, encrypt_helper};

    proptest! {
        #[test]
        fn proptest_encrypt_cycle(plaintext in prop::string::string_regex(".*").unwrap()) {
            let mut plaintext_bytes = plaintext.clone().into_bytes();

            let ciphertext = encrypt_helper(&mut plaintext_bytes, "test", 1);
            let decrypted = decrypt_helper(&ciphertext, "test").unwrap();

            prop_assert!(plaintext == decrypted);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        io::Cursor,
    };

    use indoc::indoc;
    use matrix_sdk_test::async_test;
    use ruma::room_id;

    use super::{
        decode, decrypt_helper, decrypt_room_key_export, encrypt_helper, encrypt_room_key_export,
    };
    use crate::{error::OlmResult, machine::tests::get_prepared_machine, RoomKeyImportResult};

    const PASSPHRASE: &str = "1234";

    const TEST_EXPORT: &str = indoc! {"
        -----BEGIN MEGOLM SESSION DATA-----
        Af7mGhlzQ+eGvHu93u0YXd3D/+vYMs3E7gQqOhuCtkvGAAAAASH7pEdWvFyAP1JUisAcpEo
        Xke2Q7Kr9hVl/SCc6jXBNeJCZcrUbUV4D/tRQIl3E9L4fOk928YI1J+3z96qiH0uE7hpsCI
        CkHKwjPU+0XTzFdIk1X8H7sZ+MD/2Sg/q3y8rtUjz7uEj4GUTnb+9SCOTVmJsRfqgUpM1CU
        bDLytHf1JkohY4tWEgpsCc67xdzgodjr12qYrfg/zNm3LGpxlrffJknw4rk5QFTj4kMbqbD
        ZZgDTni+HxRTDGge2J620lMOiznvXX+H09Rwruqx5aJvvaaKd86jWRpiO2oSFqHn4u5ONl9
        41uzm62Sj0eIm6ZbA9NQs87jQw4LxsejhZVL+NdjIg80zVSBTWhTdo0DTnbFSNP4ReOiz0U
        XosOF8A5T8Vdx2nvA0GXltfcHKVKQYh/LJAkNQ7P9UYL4ae/5TtQZkhB1KxCLTRWqADCl53
        uBMGpG53EMgY6G6K2DEIOkcv7sdXQF5WpemiSWZqJRWj+cjfs9BpCTbkp/rszWFl2TniWpR
        RqIbT2jORlN4rTvdtF0F4z1pqP4qWyR3sLNTkXm9CFRzWADNG0RDZKxbCoo6RPvtaCTfaHo
        SwfvzBS6CjfAG+FOugpV48o7+XetaUUPZ6/tZSPhCdeV8eP9q5r0QwWeXFogzoNzWt4HYx9
        MdXxzD+f0mtg5gzehrrEEARwI2bCvPpHxlt/Na9oW/GBpkjwR1LSKgg4CtpRyWngPjdEKpZ
        GYW19pdjg0qdXNk/eqZsQTsNWVo6A
        -----END MEGOLM SESSION DATA-----
    "};

    fn export_without_headers() -> String {
        TEST_EXPORT.lines().filter(|l| !l.starts_with("-----")).collect()
    }

    #[test]
    fn test_decode() {
        let export = export_without_headers();
        decode(export).unwrap();
    }

    #[test]
    fn test_encrypt_decrypt() {
        let data = "It's a secret to everybody";
        let mut bytes = data.to_owned().into_bytes();

        let encrypted = encrypt_helper(&mut bytes, PASSPHRASE, 10);
        let decrypted = decrypt_helper(&encrypted, PASSPHRASE).unwrap();

        assert_eq!(data, decrypted);
    }

    #[async_test]
    async fn test_session_encrypt() {
        let (machine, _) = get_prepared_machine().await;
        let room_id = room_id!("!test:localhost");

        machine.create_outbound_group_session_with_defaults(room_id).await.unwrap();
        let export = machine.export_room_keys(|s| s.room_id() == room_id).await.unwrap();

        assert!(!export.is_empty());

        let encrypted = encrypt_room_key_export(&export, "1234", 1).unwrap();
        let decrypted = decrypt_room_key_export(Cursor::new(encrypted), "1234").unwrap();

        for (exported, decrypted) in export.iter().zip(decrypted.iter()) {
            assert_eq!(exported.session_key.to_base64(), decrypted.session_key.to_base64());
        }

        assert_eq!(
            machine.import_room_keys(decrypted, false, |_, _| {}).await.unwrap(),
            RoomKeyImportResult::new(0, 1, BTreeMap::new())
        );
    }

    #[async_test]
    async fn test_importing_better_session() -> OlmResult<()> {
        let (machine, _) = get_prepared_machine().await;
        let room_id = room_id!("!test:localhost");
        let session = machine.create_inbound_session(room_id).await?;

        let export = vec![session.export_at_index(10).await];

        let keys = RoomKeyImportResult::new(
            1,
            1,
            BTreeMap::from([(
                session.room_id().to_owned(),
                BTreeMap::from([(
                    session.sender_key().to_base64(),
                    BTreeSet::from([session.session_id().to_owned()]),
                )]),
            )]),
        );

        assert_eq!(machine.import_room_keys(export, false, |_, _| {}).await?, keys);

        let export = vec![session.export_at_index(10).await];
        assert_eq!(
            machine.import_room_keys(export, false, |_, _| {}).await?,
            RoomKeyImportResult::new(0, 1, BTreeMap::new())
        );

        let better_export = vec![session.export().await];

        assert_eq!(machine.import_room_keys(better_export, false, |_, _| {}).await?, keys);

        let another_session = machine.create_inbound_session(room_id).await?;
        let export = vec![another_session.export_at_index(10).await];

        let keys = RoomKeyImportResult::new(
            1,
            1,
            BTreeMap::from([(
                another_session.room_id().to_owned(),
                BTreeMap::from([(
                    another_session.sender_key().to_base64(),
                    BTreeSet::from([another_session.session_id().to_owned()]),
                )]),
            )]),
        );

        assert_eq!(machine.import_room_keys(export, false, |_, _| {}).await?, keys);

        Ok(())
    }

    #[test]
    fn test_real_decrypt() {
        let reader = Cursor::new(TEST_EXPORT);
        let imported =
            decrypt_room_key_export(reader, PASSPHRASE).expect("Can't decrypt key export");
        assert!(!imported.is_empty())
    }
}
