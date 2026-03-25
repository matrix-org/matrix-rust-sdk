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

use std::io::{Error as IoError, Read};

use aes::{
    Aes256,
    cipher::{KeyIvInit, StreamCipher},
};
use rand::{RngCore, thread_rng};
use ruma::{
    events::room::{
        EncryptedFile, EncryptedFileHash, EncryptedFileHashAlgorithm, EncryptedFileHashes,
        EncryptedFileInfo, V2EncryptedFileInfo,
    },
    serde::Base64,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const IV_SIZE: usize = 16;
const KEY_SIZE: usize = 32;
const HASH_SIZE: usize = 32;

type Aes256Ctr = ctr::Ctr128BE<Aes256>;

/// A wrapper that transparently encrypts anything that implements `Read` as an
/// Matrix attachment.
pub struct AttachmentDecryptor<'a, R: Read> {
    inner: &'a mut R,
    expected_hash: [u8; HASH_SIZE],
    sha: Sha256,
    aes: Aes256Ctr,
}

#[cfg(not(tarpaulin_include))]
impl<'a, R: 'a + Read + std::fmt::Debug> std::fmt::Debug for AttachmentDecryptor<'a, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentDecryptor")
            .field("inner", &self.inner)
            .field("expected_hash", &self.expected_hash)
            .finish()
    }
}

impl<R: Read> Read for AttachmentDecryptor<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read_bytes = self.inner.read(buf)?;

        if read_bytes == 0 {
            let hash = self.sha.finalize_reset();

            if hash.as_slice() == self.expected_hash.as_slice() {
                Ok(0)
            } else {
                Err(IoError::other("Hash mismatch while decrypting"))
            }
        } else {
            self.sha.update(&buf[0..read_bytes]);
            self.aes.apply_keystream(&mut buf[0..read_bytes]);

            Ok(read_bytes)
        }
    }
}

/// Error type for attachment decryption.
#[derive(Error, Debug)]
pub enum DecryptorError {
    /// Some data in the encrypted attachment coldn't be decoded, this may be a
    /// hash, the secret key, or the initialization vector.
    #[error(transparent)]
    Decode(#[from] vodozemac::Base64DecodeError),
    /// A hash is missing from the encryption info.
    #[error("The encryption info is missing a hash")]
    MissingHash,
    /// The supplied data was encrypted with an unknown version of the
    /// attachment encryption spec.
    #[error("Unknown version for the encrypted attachment.")]
    UnknownVersion,
}

impl<'a, R: Read + 'a> AttachmentDecryptor<'a, R> {
    /// Wrap the given reader decrypting all the data we read from it.
    ///
    /// # Arguments
    ///
    /// * `reader` - The `Reader` that should be wrapped and decrypted.
    ///
    /// * `info` - The encryption info that is necessary to decrypt data from
    ///   the reader.
    ///
    /// # Examples
    /// ```
    /// # use std::io::{Cursor, Read};
    /// # use matrix_sdk_crypto::{AttachmentEncryptor, AttachmentDecryptor};
    /// let data = "Hello world".to_owned();
    /// let mut cursor = Cursor::new(data.clone());
    ///
    /// let mut encryptor = AttachmentEncryptor::new(&mut cursor);
    ///
    /// let mut encrypted = Vec::new();
    /// encryptor.read_to_end(&mut encrypted).unwrap();
    /// let info = encryptor.finish();
    ///
    /// let mut cursor = Cursor::new(encrypted);
    /// let mut decryptor = AttachmentDecryptor::new(&mut cursor, info).unwrap();
    /// let mut decrypted_data = Vec::new();
    /// decryptor.read_to_end(&mut decrypted_data).unwrap();
    ///
    /// let decrypted = String::from_utf8(decrypted_data).unwrap();
    /// ```
    pub fn new(
        input: &'a mut R,
        info: MediaEncryptionInfo,
    ) -> Result<AttachmentDecryptor<'a, R>, DecryptorError> {
        let EncryptedFileInfo::V2(encryption_info) = info.encryption_info else {
            return Err(DecryptorError::UnknownVersion);
        };

        let Some(EncryptedFileHash::Sha256(hash)) =
            info.hashes.get(&EncryptedFileHashAlgorithm::Sha256)
        else {
            return Err(DecryptorError::MissingHash);
        };
        let hash = hash.clone().into_inner();
        let key = encryption_info.k.as_inner();
        let iv = encryption_info.iv.as_inner();

        let sha = Sha256::default();

        let aes = Aes256Ctr::new(key.into(), iv.into());

        Ok(AttachmentDecryptor { inner: input, expected_hash: hash, sha, aes })
    }
}

/// A wrapper that transparently encrypts anything that implements `Read`.
pub struct AttachmentEncryptor<'a, R: Read + ?Sized> {
    finished: bool,
    inner: &'a mut R,
    key: [u8; KEY_SIZE],
    iv: [u8; IV_SIZE],
    hashes: EncryptedFileHashes,
    aes: Aes256Ctr,
    sha: Sha256,
}

#[cfg(not(tarpaulin_include))]
impl<'a, R: 'a + Read + std::fmt::Debug + ?Sized> std::fmt::Debug for AttachmentEncryptor<'a, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentEncryptor")
            .field("inner", &self.inner)
            .field("finished", &self.finished)
            .finish()
    }
}

impl<'a, R: Read + ?Sized + 'a> Read for AttachmentEncryptor<'a, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read_bytes = self.inner.read(buf)?;

        if read_bytes == 0 {
            Ok(0)
        } else {
            self.aes.apply_keystream(&mut buf[0..read_bytes]);
            self.sha.update(&buf[0..read_bytes]);

            Ok(read_bytes)
        }
    }
}

impl<'a, R: Read + ?Sized + 'a> AttachmentEncryptor<'a, R> {
    /// Wrap the given reader encrypting all the data we read from it.
    ///
    /// After all the reads are done, and all the data is encrypted that we wish
    /// to encrypt a call to [`finish()`](#method.finish) is necessary to get
    /// the decryption key for the data.
    ///
    /// # Arguments
    ///
    /// * `reader` - The `Reader` that should be wrapped and encrypted.
    ///
    /// # Panics
    ///
    /// Panics if we can't generate enough random data to create a fresh
    /// encryption key.
    ///
    /// # Examples
    /// ```
    /// # use std::io::{Cursor, Read};
    /// # use matrix_sdk_crypto::AttachmentEncryptor;
    /// let data = "Hello world".to_owned();
    /// let mut cursor = Cursor::new(data.clone());
    ///
    /// let mut encryptor = AttachmentEncryptor::new(&mut cursor);
    ///
    /// let mut encrypted = Vec::new();
    /// encryptor.read_to_end(&mut encrypted).unwrap();
    /// let key = encryptor.finish();
    /// ```
    pub fn new(reader: &'a mut R) -> Self {
        let mut key = [0u8; KEY_SIZE];
        let mut iv = [0u8; IV_SIZE];

        let mut rng = thread_rng();

        rng.fill_bytes(&mut key);
        // Only populate the first 8 bytes with randomness, the rest is 0
        // initialized for the counter.
        rng.fill_bytes(&mut iv[0..8]);

        let key_array = &key.into();

        let aes = Aes256Ctr::new(key_array, &iv.into());

        AttachmentEncryptor {
            finished: false,
            inner: reader,
            iv,
            key,
            hashes: EncryptedFileHashes::new(),
            aes,
            sha: Sha256::default(),
        }
    }

    /// Consume the encryptor and get the encryption key.
    pub fn finish(mut self) -> MediaEncryptionInfo {
        let hash = self.sha.finalize();
        self.hashes.insert(EncryptedFileHash::Sha256(Base64::new(hash.into())));

        MediaEncryptionInfo {
            encryption_info: V2EncryptedFileInfo::encode(self.key, self.iv).into(),
            hashes: self.hashes,
        }
    }
}

/// Struct holding all the information that is needed to decrypt an encrypted
/// file.
#[derive(Debug, Serialize, Deserialize)]
pub struct MediaEncryptionInfo {
    /// The information about the file's encryption.
    #[serde(flatten)]
    pub encryption_info: EncryptedFileInfo,
    /// The hashes that can be used to check the validity of the file.
    pub hashes: EncryptedFileHashes,
}

impl From<EncryptedFile> for MediaEncryptionInfo {
    fn from(file: EncryptedFile) -> Self {
        Self { encryption_info: file.info, hashes: file.hashes }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read};

    use serde_json::json;

    use super::{AttachmentDecryptor, AttachmentEncryptor, MediaEncryptionInfo};

    const EXAMPLE_DATA: &[u8] = &[
        179, 154, 118, 127, 186, 127, 110, 33, 203, 33, 33, 134, 67, 100, 173, 46, 235, 27, 215,
        172, 36, 26, 75, 47, 33, 160,
    ];

    fn example_key_json() -> serde_json::Value {
        json!({
            "v": "v2",
            "key": {
                "kty": "oct",
                "alg": "A256CTR",
                "ext": true,
                "k": "Voq2nkPme_x8no5-Tjq_laDAdxE6iDbxnlQXxwFPgE4",
                "key_ops": ["decrypt", "encrypt"]
            },
            "iv": "i0DovxYdJEcAAAAAAAAAAA",
            "hashes": {
                "sha256": "ANdt819a8bZl4jKy3Z+jcqtiNICa2y0AW4BBJ/iQRAU"
            }
        })
    }

    fn example_key() -> MediaEncryptionInfo {
        serde_json::from_value(example_key_json()).unwrap()
    }

    #[test]
    fn media_encryption_info_serde_roundtrip() {
        let json = example_key_json();

        let info = serde_json::from_value::<MediaEncryptionInfo>(json.clone()).unwrap();

        let serialized_info = serde_json::to_value(&info).unwrap();
        assert_eq!(serialized_info, json);
    }

    #[test]
    fn encrypt_decrypt_cycle() {
        let data = "Hello world".to_owned();
        let mut cursor = Cursor::new(data.clone());

        let mut encryptor = AttachmentEncryptor::new(&mut cursor);

        let mut encrypted = Vec::new();

        encryptor.read_to_end(&mut encrypted).unwrap();
        let key = encryptor.finish();
        assert_ne!(encrypted.as_slice(), data.as_bytes());

        let mut cursor = Cursor::new(encrypted);
        let mut decryptor = AttachmentDecryptor::new(&mut cursor, key).unwrap();
        let mut decrypted_data = Vec::new();

        decryptor.read_to_end(&mut decrypted_data).unwrap();

        let decrypted = String::from_utf8(decrypted_data).unwrap();

        assert_eq!(data, decrypted);
    }

    #[test]
    fn real_decrypt() {
        let mut cursor = Cursor::new(EXAMPLE_DATA.to_vec());
        let key = example_key();

        let mut decryptor = AttachmentDecryptor::new(&mut cursor, key).unwrap();
        let mut decrypted_data = Vec::new();

        decryptor.read_to_end(&mut decrypted_data).unwrap();
        let decrypted = String::from_utf8(decrypted_data).unwrap();

        assert_eq!("It's a secret to everybody", decrypted);
    }

    #[test]
    fn decrypt_invalid_hash() {
        let mut cursor = Cursor::new("fake message");
        let key = example_key();

        let mut decryptor = AttachmentDecryptor::new(&mut cursor, key).unwrap();
        let mut decrypted_data = Vec::new();

        decryptor.read_to_end(&mut decrypted_data).unwrap_err();
    }
}
