// Copyright 2026 The Matrix.org Foundation C.I.C.
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

//! STREAM (Rogaway et al.) based attachment encryption using
//! XChaCha20-Poly1305 as the underlying AEAD.
//!
//! STREAM splits the plaintext into fixed-size segments, each independently
//! authenticated with its own AEAD tag. This provides per-segment integrity
//! verification for file attachments, unlike the existing AES-CTR + SHA-256
//! approach which only verifies integrity at EOF.

use std::io::{self, Read};

use aead::{
    KeyInit,
    stream::{DecryptorBE32, EncryptorBE32},
};
use chacha20poly1305::XChaCha20Poly1305;
use rand::{RngCore, thread_rng};
use ruma::serde::Base64;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// Default plaintext segment size: 64 KiB.
const SEGMENT_SIZE: usize = 65536;

/// XChaCha20-Poly1305 key size (256-bit)
const KEY_SIZE: usize = 32;

/// Poly1305 authentication tag size.
const TAG_SIZE: usize = 16;

/// Nonce prefix size for the `StreamBE32` STREAM variant.
/// 24 (XChaCha20 nonce) - 5 (4-byte counter + 1-byte last-segment flag) = 19.
const NONCE_PREFIX_SIZE: usize = 19;

const VERSION: &str = "v1-stream";

/// Metadata needed to decrypt a STREAM-encrypted attachment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamMediaEncryptionInfo {
    /// Encryption scheme version identifier
    #[serde(rename = "v")]
    pub version: String,

    /// Base64-encoded 256-bit symmetric key
    pub key: Base64,

    /// Base64-encoded 19-byte nonce prefix
    pub nonce_prefix: Base64,

    /// Plaintext segment size in bytes used during encryption
    pub segment_size: u32,
}

/// State machine for the encryptor's [`Read`] impl.
#[derive(Debug)]
enum EncryptorState {
    /// Reading plaintext from the inner reader to fill a segment.
    Accumulating,
    /// A segment has been encrypted; serving ciphertext to the caller.
    /// The bool tracks whether this was the last segment.
    Serving { is_last: bool },
    /// The last segment has been encrypted and fully served.
    Done,
}

/// A reader wrapper that transparently encrypts the read contents in a
/// streaming fashion using STREAM with XChaCha20-Poly1305 as the AEAD.
///
/// Each read returns encrypted ciphertext. Call [`info`](Self::info) at any
/// time to obtain the [`StreamMediaEncryptionInfo`] needed for decryption.
///
/// # Examples
///
/// ```
/// # use std::io::{Cursor, Read};
/// # use matrix_sdk_crypto::StreamAttachmentEncryptor;
/// let data = "Hello world".to_owned();
/// let mut cursor = Cursor::new(data.clone());
///
/// let mut encryptor = StreamAttachmentEncryptor::new(&mut cursor);
///
/// // This contains information the decryptor side will need to start decryption
/// let info = encryptor.info().clone();
///
/// let mut encrypted = Vec::new();
/// encryptor.read_to_end(&mut encrypted).unwrap();
/// ```
pub struct StreamAttachmentEncryptor<'a, R: Read + ?Sized> {
    inner: &'a mut R,
    encryptor: Option<EncryptorBE32<XChaCha20Poly1305>>,
    plaintext_buf: Vec<u8>,
    ciphertext_buf: Vec<u8>,
    ct_pos: usize,
    state: EncryptorState,
    info: StreamMediaEncryptionInfo,
}

impl<R: Read + ?Sized + std::fmt::Debug> std::fmt::Debug for StreamAttachmentEncryptor<'_, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamAttachmentEncryptor")
            .field("inner", &self.inner)
            .field("state", &self.state)
            .finish()
    }
}

impl<'a, R: Read + ?Sized + 'a> StreamAttachmentEncryptor<'a, R> {
    /// Wrap the given reader, encrypting all data read from it using STREAM
    /// with XChaCha20-Poly1305.
    pub fn new(reader: &'a mut R) -> Self {
        Self::with_segment_size(reader, SEGMENT_SIZE as u32)
    }

    /// Like [`new`](Self::new) but with a custom plaintext segment size.
    pub fn with_segment_size(reader: &'a mut R, segment_size: u32) -> Self {
        let mut key = [0u8; KEY_SIZE];
        let mut nonce_prefix = [0u8; NONCE_PREFIX_SIZE];

        let mut rng = thread_rng();
        rng.fill_bytes(&mut key);
        rng.fill_bytes(&mut nonce_prefix);

        let cipher = XChaCha20Poly1305::new((&key).into());
        let encryptor = EncryptorBE32::from_aead(cipher, (&nonce_prefix).into());

        let info = StreamMediaEncryptionInfo {
            version: VERSION.to_owned(),
            key: Base64::new(key.to_vec()),
            nonce_prefix: Base64::new(nonce_prefix.to_vec()),
            segment_size,
        };

        key.zeroize();

        StreamAttachmentEncryptor {
            inner: reader,
            encryptor: Some(encryptor),
            plaintext_buf: Vec::with_capacity(segment_size as usize),
            ciphertext_buf: Vec::new(),
            ct_pos: 0,
            state: EncryptorState::Accumulating,
            info,
        }
    }

    #[cfg(test)]
    fn with_key_and_nonce(
        reader: &'a mut R,
        key: &[u8; KEY_SIZE],
        nonce_prefix: &[u8; NONCE_PREFIX_SIZE],
    ) -> Self {
        let cipher = XChaCha20Poly1305::new(key.into());
        let encryptor = EncryptorBE32::from_aead(cipher, nonce_prefix.into());

        let info = StreamMediaEncryptionInfo {
            version: VERSION.to_owned(),
            key: Base64::new(key.to_vec()),
            nonce_prefix: Base64::new(nonce_prefix.to_vec()),
            segment_size: SEGMENT_SIZE as u32,
        };

        StreamAttachmentEncryptor {
            inner: reader,
            encryptor: Some(encryptor),
            plaintext_buf: Vec::with_capacity(SEGMENT_SIZE),
            ciphertext_buf: Vec::new(),
            ct_pos: 0,
            state: EncryptorState::Accumulating,
            info,
        }
    }

    /// Return the encryption metadata needed for decryption.
    ///
    /// This is available immediately after the encryptor is constructed.
    pub fn info(&self) -> &StreamMediaEncryptionInfo {
        &self.info
    }

    /// Fill the plaintext buffer from the inner reader until a full segment
    /// is accumulated or the inner reader reaches EOF.
    ///
    /// Returns `true` if the inner reader reached EOF.
    fn fill_plaintext_buf(&mut self) -> io::Result<bool> {
        let segment_size = self.info.segment_size as usize;
        let mut tmp = [0u8; 8192];

        while self.plaintext_buf.len() < segment_size {
            let max = tmp.len().min(segment_size - self.plaintext_buf.len());
            match self.inner.read(&mut tmp[..max]) {
                Ok(0) => return Ok(true),
                Ok(n) => self.plaintext_buf.extend_from_slice(&tmp[..n]),
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }

        Ok(false)
    }
}

impl<R: Read + ?Sized> Read for StreamAttachmentEncryptor<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.state {
                EncryptorState::Accumulating => {
                    let inner_eof = self.fill_plaintext_buf()?;
                    let segment_size = self.info.segment_size as usize;

                    if inner_eof {
                        // Inner reader is done. Encrypt as last segment
                        // (possibly empty, if input was an exact multiple
                        // of segment_size).
                        let encryptor = self.encryptor.take().ok_or_else(|| {
                            io::Error::other("STREAM encryptor already finalized")
                        })?;

                        self.ciphertext_buf =
                            encryptor.encrypt_last(&self.plaintext_buf[..]).map_err(|e| {
                                io::Error::other(format!("STREAM encryption error: {e}"))
                            })?;
                        self.plaintext_buf.zeroize();
                        self.state = EncryptorState::Serving { is_last: true };
                    } else if self.plaintext_buf.len() == segment_size {
                        // Full intermediate segment.
                        let encryptor = self.encryptor.as_mut().ok_or_else(|| {
                            io::Error::other("STREAM encryptor already finalized")
                        })?;

                        self.ciphertext_buf =
                            encryptor.encrypt_next(&self.plaintext_buf[..]).map_err(|e| {
                                io::Error::other(format!("STREAM encryption error: {e}"))
                            })?;
                        self.plaintext_buf.zeroize();
                        self.state = EncryptorState::Serving { is_last: false };
                    }
                    // No return; we deliberately loop to transition to the next
                    // state, which is serving from the freshly filled
                    // ciphertext buffer.
                }

                EncryptorState::Serving { is_last } => {
                    let available = &self.ciphertext_buf[self.ct_pos..];
                    if available.is_empty() {
                        // Buffer fully consumed so we need to transition to the next state.
                        self.ciphertext_buf.clear();
                        self.ct_pos = 0;
                        self.state = if is_last {
                            EncryptorState::Done
                        } else {
                            EncryptorState::Accumulating
                        };
                        continue;
                    }

                    let to_copy = available.len().min(buf.len());
                    buf[..to_copy].copy_from_slice(&available[..to_copy]);
                    self.ct_pos += to_copy;
                    return Ok(to_copy);
                }

                EncryptorState::Done => return Ok(0),
            }
        }
    }
}

/// State machine for the decryptor's [`Read`] impl.
#[derive(Debug)]
enum DecryptorState {
    /// Accumulating ciphertext from the inner reader to fill a segment.
    Accumulating,
    /// A segment has been decrypted; serving plaintext to the caller.
    /// The bool tracks whether this was the last segment.
    Serving { is_last: bool },
    /// The last segment has been decrypted and fully served.
    Done,
}

/// A reader wrapper that transparently decrypts the read contents,
/// reversing the encryption performed by [`StreamAttachmentEncryptor`].
///
/// Each read returns decrypted plaintext. Authentication is verified
/// per-segment: a tampered segment causes an immediate error rather than
/// waiting until EOF.
///
/// # Examples
///
/// ```
/// # use std::io::{Cursor, Read};
/// # use matrix_sdk_crypto::{StreamAttachmentEncryptor, StreamAttachmentDecryptor};
/// let data = "Hello world".to_owned();
/// let mut cursor = Cursor::new(data.clone());
///
/// let mut encryptor = StreamAttachmentEncryptor::new(&mut cursor);
/// let info = encryptor.info().clone();
/// let mut encrypted = Vec::new();
/// encryptor.read_to_end(&mut encrypted).unwrap();
///
/// let mut cursor = Cursor::new(encrypted);
/// let mut decryptor = StreamAttachmentDecryptor::new(&mut cursor, info).unwrap();
/// let mut decrypted = Vec::new();
/// decryptor.read_to_end(&mut decrypted).unwrap();
/// assert_eq!(data.as_bytes(), &decrypted[..]);
/// ```
pub struct StreamAttachmentDecryptor<'a, R: Read> {
    inner: &'a mut R,
    decryptor: Option<DecryptorBE32<XChaCha20Poly1305>>,
    segment_buf: Vec<u8>,
    plaintext_buf: Vec<u8>,
    pt_pos: usize,
    state: DecryptorState,
}

impl<R: Read + std::fmt::Debug> std::fmt::Debug for StreamAttachmentDecryptor<'_, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamAttachmentDecryptor")
            .field("inner", &self.inner)
            .field("state", &self.state)
            .finish()
    }
}

/// Error type for STREAM attachment decryption.
#[derive(Debug, thiserror::Error)]
pub enum StreamDecryptorError {
    /// The version field doesn't match the expected value.
    #[error("Unknown version for STREAM-encrypted attachment: {0}")]
    UnknownVersion(String),

    /// The supplied key has an invalid length.
    #[error("Invalid key length: expected {expected}, got {got}")]
    InvalidKeyLength {
        /// Expected key length in bytes.
        expected: usize,
        /// Actual key length in bytes.
        got: usize,
    },

    /// The supplied nonce prefix has an invalid length.
    #[error("Invalid nonce prefix length: expected {expected}, got {got}")]
    InvalidNoncePrefixLength {
        /// Expected nonce prefix length in bytes.
        expected: usize,
        /// Actual nonce prefix length in bytes.
        got: usize,
    },
}

impl<'a, R: Read + 'a> StreamAttachmentDecryptor<'a, R> {
    /// Wrap the given reader, decrypting STREAM-encrypted data on the fly.
    pub fn new(
        input: &'a mut R,
        info: StreamMediaEncryptionInfo,
    ) -> Result<Self, StreamDecryptorError> {
        if info.version != VERSION {
            return Err(StreamDecryptorError::UnknownVersion(info.version));
        }

        let key_bytes = info.key.as_bytes();
        if key_bytes.len() != KEY_SIZE {
            return Err(StreamDecryptorError::InvalidKeyLength {
                expected: KEY_SIZE,
                got: key_bytes.len(),
            });
        }

        let nonce_bytes = info.nonce_prefix.as_bytes();
        if nonce_bytes.len() != NONCE_PREFIX_SIZE {
            return Err(StreamDecryptorError::InvalidNoncePrefixLength {
                expected: NONCE_PREFIX_SIZE,
                got: nonce_bytes.len(),
            });
        }

        let cipher = XChaCha20Poly1305::new(key_bytes.into());
        let decryptor = DecryptorBE32::from_aead(cipher, nonce_bytes.into());

        Ok(StreamAttachmentDecryptor {
            inner: input,
            decryptor: Some(decryptor),
            segment_buf: vec![0u8; info.segment_size as usize + TAG_SIZE],
            plaintext_buf: Vec::new(),
            pt_pos: 0,
            state: DecryptorState::Accumulating,
        })
    }
}

impl<R: Read> Read for StreamAttachmentDecryptor<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.state {
                DecryptorState::Accumulating => {
                    // Read one encrypted segment from the inner reader.
                    let encrypted_segment_size = self.segment_buf.len();
                    let mut total_read = 0;

                    while total_read < encrypted_segment_size {
                        match self.inner.read(&mut self.segment_buf[total_read..]) {
                            Ok(0) => break,
                            Ok(n) => total_read += n,
                            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                            Err(e) => return Err(e),
                        }
                    }

                    if total_read == 0 {
                        // If we are in the Accumulating state, we will either
                        // encounter a final segment (so total_read will be > 0)
                        // or the stream will have been truncated.
                        return Err(io::Error::other(
                            "STREAM decryption error: unexpected end of data (truncated stream)",
                        ));
                    }

                    let is_last = total_read < encrypted_segment_size;

                    if is_last {
                        let decryptor = self.decryptor.take().ok_or_else(|| {
                            io::Error::other("STREAM decryptor already finalized")
                        })?;

                        self.plaintext_buf = decryptor
                            .decrypt_last(&self.segment_buf[..total_read])
                            .map_err(|_| {
                                io::Error::other("STREAM decryption error: authentication failed")
                            })?;
                    } else {
                        let decryptor = self.decryptor.as_mut().ok_or_else(|| {
                            io::Error::other("STREAM decryptor already finalized")
                        })?;

                        self.plaintext_buf = decryptor
                            .decrypt_next(&self.segment_buf[..])
                            .map_err(|_| {
                                io::Error::other("STREAM decryption error: authentication failed")
                            })?;
                    }

                    self.pt_pos = 0;
                    self.state = DecryptorState::Serving { is_last };
                    // No return; we deliberately loop to transition to the next
                    // state, which is serving from the freshly filled
                    // plaintext buffer.
                }

                DecryptorState::Serving { is_last } => {
                    let available = &self.plaintext_buf[self.pt_pos..];
                    if available.is_empty() {
                        // Buffer fully consumed so we need to transition to the next state.
                        self.plaintext_buf.zeroize();
                        self.pt_pos = 0;
                        self.state = if is_last {
                            DecryptorState::Done
                        } else {
                            DecryptorState::Accumulating
                        };
                        continue;
                    }

                    let to_copy = available.len().min(buf.len());
                    buf[..to_copy].copy_from_slice(&available[..to_copy]);
                    self.pt_pos += to_copy;
                    return Ok(to_copy);
                }

                DecryptorState::Done => return Ok(0),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read};

    use super::*;

    /// Helper: encrypt data and return (ciphertext, info).
    fn encrypt(data: &[u8]) -> (Vec<u8>, StreamMediaEncryptionInfo) {
        let mut cursor = Cursor::new(data);
        let mut encryptor = StreamAttachmentEncryptor::new(&mut cursor);
        let info = encryptor.info().clone();
        let mut encrypted = Vec::new();
        encryptor.read_to_end(&mut encrypted).unwrap();
        (encrypted, info)
    }

    /// Helper: decrypt ciphertext with info, returning plaintext.
    fn decrypt(ciphertext: &[u8], info: StreamMediaEncryptionInfo) -> Result<Vec<u8>, io::Error> {
        let mut cursor = Cursor::new(ciphertext);
        let mut decryptor = StreamAttachmentDecryptor::new(&mut cursor, info)
            .map_err(|e| io::Error::other(e.to_string()))?;
        let mut decrypted = Vec::new();
        decryptor.read_to_end(&mut decrypted)?;
        Ok(decrypted)
    }

    // Functional tests

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let data = b"Hello world";
        let (encrypted, info) = encrypt(data);

        assert_ne!(&encrypted[..], data);

        let decrypted = decrypt(&encrypted, info).unwrap();
        assert_eq!(&decrypted[..], data);
    }

    #[test]
    fn large_data_roundtrip() {
        let mut data = vec![0u8; 1024 * 1024]; // 1 MiB
        let mut rng = thread_rng();
        rng.fill_bytes(&mut data);

        let (encrypted, info) = encrypt(&data);
        let decrypted = decrypt(&encrypted, info).unwrap();
        assert_eq!(decrypted, data);
    }

    #[test]
    fn segment_boundary_exact_roundtrip() {
        let data = vec![0xAB; SEGMENT_SIZE];
        let (encrypted, info) = encrypt(&data);
        let decrypted = decrypt(&encrypted, info).unwrap();
        assert_eq!(decrypted, data);
    }

    #[test]
    fn segment_boundary_minus_one_roundtrip() {
        let data = vec![0xAB; SEGMENT_SIZE - 1];
        let (encrypted, info) = encrypt(&data);
        let decrypted = decrypt(&encrypted, info).unwrap();
        assert_eq!(decrypted, data);
    }

    #[test]
    fn segment_boundary_plus_one_roundtrip() {
        let data = vec![0xAB; SEGMENT_SIZE + 1];
        let (encrypted, info) = encrypt(&data);
        let decrypted = decrypt(&encrypted, info).unwrap();
        assert_eq!(decrypted, data);
    }

    #[test]
    fn empty_input_roundtrip() {
        let data = b"";
        let (encrypted, info) = encrypt(data);
        let decrypted = decrypt(&encrypted, info).unwrap();
        assert_eq!(&decrypted[..], data.as_slice());
    }

    #[test]
    fn small_read_buffer() {
        let data = b"It is I, Sahasrahla";
        let (encrypted, info) = encrypt(data);

        let mut cursor = Cursor::new(&encrypted);
        let mut decryptor = StreamAttachmentDecryptor::new(&mut cursor, info).unwrap();

        // Read one byte at a time.
        let mut decrypted = Vec::new();
        let mut one_byte = [0u8; 1];
        loop {
            match decryptor.read(&mut one_byte) {
                Ok(0) => break,
                Ok(n) => {
                    decrypted.extend_from_slice(&one_byte[..n]);
                    assert_eq!(n, 1);
                }
                Err(e) => panic!("unexpected error: {e}"),
            }
        }

        assert_eq!(&decrypted[..], data.as_slice());
    }

    // Security tests

    #[test]
    fn tampering_detection() {
        // Create data spanning multiple segments.
        let data = vec![0xAB; SEGMENT_SIZE * 3];
        let (mut encrypted, info) = encrypt(&data);

        // Tamper with a byte in the second segment.
        let second_segment_offset = SEGMENT_SIZE + TAG_SIZE + 10;
        if second_segment_offset < encrypted.len() {
            encrypted[second_segment_offset] ^= 0xFF;
        }

        let result = decrypt(&encrypted, info);
        assert!(result.is_err(), "Decryption should fail on tampered data");
    }

    #[test]
    fn truncation_detection() {
        let data = vec![0xAB; SEGMENT_SIZE * 2];
        let (encrypted, info) = encrypt(&data);

        // Truncate to just the first segment.
        let truncated = &encrypted[..SEGMENT_SIZE + TAG_SIZE];
        let result = decrypt(truncated, info);
        assert!(result.is_err(), "Decryption should fail on truncated data");
    }

    #[test]
    fn reordering_detection() {
        // Create data with 3 segments to swap segments 2 and 1
        let data = vec![0xAB; SEGMENT_SIZE * 3];
        let (encrypted, info) = encrypt(&data);

        let ct_segment = SEGMENT_SIZE + TAG_SIZE;
        let mut reordered = Vec::new();
        // Segment 0 stays in place.
        reordered.extend_from_slice(&encrypted[..ct_segment]);
        // Swap segment 1 and segment 2.
        let segment1 = &encrypted[ct_segment..ct_segment * 2];
        let segment2 = &encrypted[ct_segment * 2..];
        reordered.extend_from_slice(segment2);
        reordered.extend_from_slice(segment1);

        let result = decrypt(&reordered, info);
        assert!(result.is_err(), "Decryption should fail on reordered segments");
    }

    // Metamorphic / property tests

    #[test]
    fn length_relationship() {
        for &size in
            &[0, 1, SEGMENT_SIZE - 1, SEGMENT_SIZE, SEGMENT_SIZE + 1, SEGMENT_SIZE * 3 + 42]
        {
            let data = vec![0xAB; size];
            let (encrypted, _info) = encrypt(&data);

            // When size is an exact multiple of SEGMENT_SIZE (and > 0), the
            // encryptor can't know the inner reader is exhausted until it
            // tries to read again, so it emits an intermediate segment followed
            // by an empty last segment. Hence: size / SEGMENT_SIZE + 1.
            let num_segments = if size == 0 {
                1 // empty input still produces one (empty) last segment with a tag
            } else {
                size / SEGMENT_SIZE + 1
            };

            let expected_ct_len = size + num_segments * TAG_SIZE;
            assert_eq!(
                encrypted.len(),
                expected_ct_len,
                "Length mismatch for plaintext size {size}: \
                 expected {expected_ct_len}, got {}",
                encrypted.len()
            );
        }
    }

    #[test]
    fn prefix_non_stability() {
        // Encrypting a prefix of some data (e.g. data[..n], for some n) MUST NOT produce a prefix
        // of encrypting data as a whole, because STREAM includes the "last segment" flag as part
        // of the nonce. 
        let key = [0x42u8; KEY_SIZE];
        let nonce_prefix = [0x13u8; NONCE_PREFIX_SIZE];

        let full_data = vec![0xAB; SEGMENT_SIZE + 100];
        let prefix_data = &full_data[..SEGMENT_SIZE]; // exactly one segment

        let mut cursor_full = Cursor::new(&full_data);
        let mut enc_full =
            StreamAttachmentEncryptor::with_key_and_nonce(&mut cursor_full, &key, &nonce_prefix);
        let mut ct_full = Vec::new();
        enc_full.read_to_end(&mut ct_full).unwrap();

        let mut cursor_prefix = Cursor::new(prefix_data);
        let mut enc_prefix =
            StreamAttachmentEncryptor::with_key_and_nonce(&mut cursor_prefix, &key, &nonce_prefix);
        let mut ct_prefix = Vec::new();
        enc_prefix.read_to_end(&mut ct_prefix).unwrap();

        // The prefix ciphertext is one segment encrypted as "last".
        // The full ciphertext's first segment is encrypted as "not last".
        // They must differ.
        let first_segment_of_full = &ct_full[..SEGMENT_SIZE + TAG_SIZE];
        assert_ne!(
            first_segment_of_full,
            &ct_prefix[..],
            "Prefix encryption must differ from the corresponding segment \
             in the full encryption (due to nonce difference because of the last-segment flag)"
        );
    }

    #[test]
    fn deterministic_with_fixed_key_nonce() {
        let key = [0x42u8; KEY_SIZE];
        let nonce_prefix = [0x13u8; NONCE_PREFIX_SIZE];
        let data = b"It's dangerous to go alone; take this!";

        let mut cursor1 = Cursor::new(data.as_slice());
        let mut enc1 =
            StreamAttachmentEncryptor::with_key_and_nonce(&mut cursor1, &key, &nonce_prefix);
        let mut ct1 = Vec::new();
        enc1.read_to_end(&mut ct1).unwrap();

        let mut cursor2 = Cursor::new(data.as_slice());
        let mut enc2 =
            StreamAttachmentEncryptor::with_key_and_nonce(&mut cursor2, &key, &nonce_prefix);
        let mut ct2 = Vec::new();
        enc2.read_to_end(&mut ct2).unwrap();

        assert_eq!(ct1, ct2, "Same key + nonce + plaintext must produce identical ciphertext");
    }
}
