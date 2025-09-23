// Copyright 2015 The adb-remote-control Developers
// Copyright 2025 The Matrix.org Foundation C.I.C.
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

// This file originates from the rust-aes-stream repo[1], it has been moddified
// to use AES-CTR mode and to authenticate the encrypted files.
// [1] https://github.com/oberien/rust-aes-stream/

//! Read/Write Wrapper for AES Encryption and Decryption during I/O Operations
use std::{
    cmp,
    convert::TryFrom,
    io::{Error, Read, Result, Seek, SeekFrom, Write},
    ops::Neg,
};

use aes::cipher::{KeyInit, KeyIvInit, StreamCipher};
use hmac::Mac;
use rand::{Rng, thread_rng};
use sha2::digest::{FixedOutputReset, Update};

const BUFFER_SIZE: usize = 8192;

/// Wraps a [`Write`] implementation with a [`StreamCipher`], additionally
/// authenticates the writer with the given [`Mac`]
pub struct AesWriter<
    E: StreamCipher + KeyIvInit + Sync + Send,
    M: Mac + KeyInit + Update + FixedOutputReset + Sync + Send,
    W: Write + Sync + Send,
> {
    /// Writer to write encrypted data to
    writer: W,
    /// Encryptor to encrypt data with
    enc: E,
    mac: M,
    finalized: bool,
}

impl<
    E: StreamCipher + KeyIvInit + Sync + Send,
    M: Mac + KeyInit + Update + FixedOutputReset + Sync + Send,
    W: Write + Sync + Send,
> AesWriter<E, M, W>
{
    /// Creates a new AesWriter with a random IV.
    ///
    /// The IV will be written as first block of the file.
    ///
    /// The MAC will be written at the end of the file.
    ///
    /// # Arguments
    ///
    /// * `writer`: Writer to write encrypted data into
    /// * `key`: The encryption key for the stream cipher.
    /// * `mac_key`: The authentication key for the MAC.
    /// * `iv_size`: The size of the initialization vector or nonce for the
    ///   stream cipher.
    pub fn new(
        mut writer: W,
        key: &[u8],
        mac_key: &[u8],
        iv_size: usize,
    ) -> Result<AesWriter<E, M, W>> {
        let mut iv = vec![0u8; iv_size];
        let mut rng = thread_rng();
        rng.try_fill(&mut iv[0..iv_size / 2])
            .map_err(|e| Error::other(format!("error generating iv: {:?}", e)))?;

        let mac = <M as Mac>::new_from_slice(mac_key)
            .map_err(|e| Error::other(format!("error creating mac: {:?}", e)))?;

        let enc = E::new_from_slices(key, &iv)
            .map_err(|e| Error::other(format!("error initializing cipher: {:?}", e)))?;
        writer.write_all(&iv)?;
        Ok(AesWriter { writer, enc, mac, finalized: false })
    }

    /// Encrypts the passed buffer and writes all resulting encrypted blocks to
    /// the underlying writer
    ///
    /// # Arguments
    ///
    /// * `buf`: Plaintext to encrypt and write.
    fn encrypt_write(&mut self, buf: &mut [u8]) -> Result<usize> {
        if self.finalized {
            return Err(Error::other("File has been already finalized"));
        }

        self.enc
            .try_apply_keystream(buf)
            .map_err(|_| Error::other("Encryption error, reached end of the keystream."))?;
        self.writer.write_all(buf)?;
        <M as Mac>::update(&mut self.mac, buf);

        Ok(buf.len())
    }

    /// Finalize the file and mark it so no more writes can happen.
    pub fn finalize(&mut self) -> Result<()> {
        // If our encryptor is using padding this write will insert it now.
        // Otherwise it will do nothing.
        self.encrypt_write(&mut [])?;

        // Write our mac after our encrypted data.
        let mac_result = <M as Mac>::finalize_reset(&mut self.mac).into_bytes();
        self.writer.write_all(mac_result.as_slice())?;

        // Mark the file as finalized and flush our underlying writer.
        self.finalized = true;

        Ok(())
    }
}

impl<
    E: StreamCipher + KeyIvInit + Sync + Send,
    M: Mac + KeyInit + Update + FixedOutputReset + Sync + Send,
    W: Write + Sync + Send,
> Write for AesWriter<E, M, W>
{
    /// Encrypts the passed buffer and writes the result to the underlying
    /// writer.
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let mut buf = buf.to_owned();
        let written = self.encrypt_write(&mut buf)?;
        Ok(written)
    }

    /// Flush this output stream, ensuring that all intermediately buffered
    /// contents reach their destination. [Read more](https://doc.rust-lang.org/nightly/std/io/trait.Write.html#tymethod.flush)
    fn flush(&mut self) -> Result<()> {
        self.writer.flush()
    }
}

impl<
    E: StreamCipher + KeyIvInit + Sync + Send,
    M: Mac + KeyInit + Update + FixedOutputReset + Sync + Send,
    W: Write + Sync + Send,
> Drop for AesWriter<E, M, W>
{
    /// Drop our AesWriter adding the MAC at the end of the file and flushing
    /// our buffers.
    fn drop(&mut self) {
        if self.finalized {
            return;
        }

        if std::thread::panicking() {
            let _ = self.finalize();
        } else {
            self.finalize().unwrap();
        }
    }
}

/// Wraps a [`Read`] implementation with a [`StreamCipher`]
pub struct AesReader<D: StreamCipher + KeyIvInit, R: Read + Seek + Clone> {
    /// Reader to read encrypted data from
    reader: R,
    /// Decryptor to decrypt data with
    dec: D,
    /// Total length of the reader
    pub(crate) length: u64,
    /// Length of the MAC.
    pub(crate) mac_length: u64,
}

impl<D: StreamCipher + KeyIvInit, R: Read + Seek + Clone> AesReader<D, R> {
    /// Creates a new AesReader.
    ///
    /// Assumes that the first block of given reader is the IV.
    ///
    /// # Arguments
    ///
    /// * `reader`: Reader to read encrypted data from
    /// * `key`: The decryption key for the stream cipher.
    /// * `mac_key`: The authentication key for the MAC.
    /// * `iv_size`: The size of the initialization vector or nonce for the
    ///   stream cipher.
    pub fn new<M: Mac + KeyInit + Update + FixedOutputReset>(
        mut reader: R,
        key: &[u8],
        mac_key: &[u8],
        iv_size: usize,
        mac_size: usize,
    ) -> Result<AesReader<D, R>> {
        let iv_length = iv_size;

        let mut mac = <M as Mac>::new_from_slice(mac_key)
            .map_err(|e| Error::other(format!("error creating mac: {:?}", e)))?;

        let mac_length = mac_size;

        let u_iv_length =
            u64::try_from(iv_length).map_err(|_| Error::other("IV length is too big"))?;
        let u_mac_length =
            u64::try_from(mac_length).map_err(|_| Error::other("MAC length is too big"))?;
        let i_mac_length =
            i64::try_from(mac_length).map_err(|_| Error::other("MAC length is too big"))?;

        let mut iv = vec![0u8; iv_length];
        let mut expected_mac = vec![0u8; mac_length];

        reader.read_exact(&mut iv)?;
        let end = reader.seek(SeekFrom::End(0))?;

        if end < (u_iv_length + u_mac_length) {
            return Err(Error::other("File doesn't contain a valid IV or MAC"));
        }

        let seek_back = i_mac_length.neg();
        reader.seek(SeekFrom::End(seek_back))?;
        reader.read_exact(&mut expected_mac)?;

        reader.seek(SeekFrom::Start(u_iv_length))?;

        let mut buffer = [0u8; BUFFER_SIZE];

        loop {
            let read =
                AesReader::<D, R>::read_until_mac(&mut buffer, &mut reader, end, u_mac_length)?;

            if read == 0 {
                break;
            }

            <M as Mac>::update(&mut mac, &buffer[..read]);
        }

        if mac.verify_slice(&expected_mac).is_err() {
            return Err(Error::other("Invalid MAC"));
        }

        reader.seek(SeekFrom::Start(u_iv_length))?;
        let dec = D::new_from_slices(key, &iv)
            .map_err(|e| Error::other(format!("couldn't initialize cipher {:?}", e)))?;

        Ok(AesReader { reader, dec, length: end, mac_length: u_mac_length })
    }

    /// Read bytes from a reader and put them into the given buffer, make sure
    /// to not read the MAC.
    ///
    /// # Arguments
    ///
    /// * `buffer`: The buffer to fill.
    /// * `reader`: Reader to read encrypted data from
    /// * `total_length`: The total number of bytes that the reader contains.
    /// * `mac_length`: The length of the MAC that is stored the file we are
    ///   reading from.
    fn read_until_mac(
        buffer: &mut [u8],
        reader: &mut R,
        total_length: u64,
        mac_length: u64,
    ) -> Result<usize> {
        let current_pos = reader.stream_position()?;
        let mac_start = total_length - mac_length;

        if current_pos >= mac_start {
            return Ok(0);
        }

        let max_to_read = cmp::min(buffer.len(), (mac_start - current_pos) as usize);
        let read = reader.read(&mut buffer[..max_to_read])?;

        Ok(read)
    }

    /// Reads and decrypts data from the underlying stream and writes it into
    /// the passed buffer.
    ///
    /// # Arguments
    ///
    /// * `buf`: Buffer to write decrypted data into.
    fn read_decrypt(&mut self, buf: &mut [u8]) -> Result<usize> {
        let read =
            AesReader::<D, R>::read_until_mac(buf, &mut self.reader, self.length, self.mac_length)?;
        self.dec
            .try_apply_keystream(buf)
            .map_err(|_| Error::other("Decryption error, reached end of the keystream."))?;
        Ok(read)
    }
}

impl<D: StreamCipher + KeyIvInit, R: Read + Seek + Clone> Read for AesReader<D, R> {
    /// Reads encrypted data from the underlying reader, decrypts it and writes
    /// the result into the passed buffer.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let read = self.read_decrypt(buf)?;
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read, Seek, Write};

    use aes::Aes128;
    use hmac::Hmac;
    use sha2::Sha256;

    use super::{AesReader, AesWriter};

    type Aes128Ctr = ctr::Ctr128BE<Aes128>;

    fn encrypt(data: &[u8]) -> Vec<u8> {
        let key = [0u8; 16];
        let hmac_key = [0u8; 16];

        let mut enc = Vec::new();
        {
            let mut aes =
                AesWriter::<Aes128Ctr, Hmac<Sha256>, _>::new(&mut enc, &key, &hmac_key, 16)
                    .unwrap();
            aes.write_all(data).unwrap();
        }
        enc
    }

    fn decrypt<R: Read + Seek + Clone>(data: R) -> Vec<u8> {
        let key = [0u8; 16];
        let mut dec = Vec::new();
        let mut aes =
            AesReader::<Aes128Ctr, _>::new::<Hmac<Sha256>>(data, &key, &key, 16, 32).unwrap();
        aes.read_to_end(&mut dec).unwrap();
        dec
    }

    #[test]
    fn enc_unaligned() {
        let orig = [0u8; 16];
        let key = [0u8; 16];
        let hmac_key = [0u8; 16];

        let mut enc = Vec::new();
        {
            let mut aes =
                AesWriter::<Aes128Ctr, Hmac<Sha256>, _>::new(&mut enc, &key, &hmac_key, 16)
                    .unwrap();
            for chunk in orig.chunks(3) {
                aes.write_all(chunk).unwrap();
            }
        }
        let dec = decrypt(Cursor::new(&enc));
        assert_eq!(dec, &orig);
    }

    #[test]
    fn enc_dec_single() {
        let orig = [0u8; 16];
        let enc = encrypt(&orig);
        let dec = decrypt(Cursor::new(&enc));
        assert_eq!(dec, &orig);
    }

    #[test]
    fn enc_dec_single_full() {
        let orig = [0u8; 16];
        let enc = encrypt(&orig);
        let dec = decrypt(Cursor::new(&enc));
        assert_eq!(dec, &orig);
    }

    #[test]
    fn dec_read_unaligned() {
        let orig = [0u8; 16];
        let enc = encrypt(&orig);

        let key = [0u8; 16];
        let mut dec: Vec<u8> = Vec::new();
        let mut aes =
            AesReader::<Aes128Ctr, _>::new::<Hmac<Sha256>>(Cursor::new(&enc), &key, &key, 16, 32)
                .unwrap();
        loop {
            let mut buf = [0u8; 3];
            let read = aes.read(&mut buf).unwrap();
            dec.extend(&buf[..read]);
            if read == 0 {
                break;
            }
        }
        assert_eq!(dec, &orig);
    }
}
