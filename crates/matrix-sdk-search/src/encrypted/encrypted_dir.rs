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

use std::{
    fmt,
    fs::create_dir_all,
    io::{BufWriter, Cursor, Error as IoError, Read, Write},
    ops::Range,
    path::Path,
    sync::Arc,
};

use aes::{
    Aes256,
    cipher::{KeyInit, KeyIvInit, StreamCipher, StreamCipherSeek},
};
use hkdf::Hkdf;
use hmac::{HmacReset, Mac};
use sha2::{
    Sha256, Sha512,
    digest::{FixedOutputReset, Update},
};
use tantivy::{
    HasLen,
    directory::{
        AntiCallToken, Directory, DirectoryLock, FileHandle, FileSlice, Lock, OwnedBytes,
        TerminatingWrite, WatchCallback, WatchHandle, WritePtr,
        error::{DeleteError, LockError, OpenDirectoryError, OpenReadError, OpenWriteError},
    },
};
use zeroize::Zeroizing;

type Aes256Ctr = ctr::Ctr128BE<Aes256>;

use crate::encrypted::encrypted_stream::{AesReader, AesWriter};

/// KeyBuffer type that makes sure that the buffer is zeroed out before being
/// dropped.
type KeyBuffer = Zeroizing<Vec<u8>>;

// 16 byte random IV for the AES-CTR mode.
const IV_SIZE: usize = 16;
// 32 byte or 256 bit encryption keys.
const KEY_SIZE: usize = 32;
// 32 byte message authentication code since HMAC-SHA256 is used.
const MAC_LENGTH: usize = 32;
// HKDF info, making the derived keys unique to the search index.
const HKDF_INFO: &[u8] = b"matrix-sdk-search-encryption-key";

trait IntoTvError<E> {
    fn into_tv_err(self, path: &Path) -> E;
}

impl IntoTvError<OpenDirectoryError> for IoError {
    fn into_tv_err(self, path: &Path) -> OpenDirectoryError {
        OpenDirectoryError::wrap_io_error(self, path.to_path_buf())
    }
}

impl IntoTvError<OpenReadError> for IoError {
    fn into_tv_err(self, path: &Path) -> OpenReadError {
        OpenReadError::wrap_io_error(self, path.to_path_buf())
    }
}

impl IntoTvError<OpenWriteError> for IoError {
    fn into_tv_err(self, path: &Path) -> OpenWriteError {
        OpenWriteError::wrap_io_error(self, path.to_path_buf())
    }
}

#[derive(Clone, Debug)]
/// A Directory implementation that wraps a MmapDirectory and adds [AES][aes]
/// based encryption to the file read/write operations.
///
/// The key must be a high-entropy key: it is expanded, without any key
/// stretching, into a 256 bit encryption key and a 256 bit MAC key using
/// [HKDF][hkdf]:
///
/// ```text
///     encryption_key, mac_key = HKDF(SHA512, key, info = "matrix-sdk-search-encryption-key", 512)
/// ```
///
/// Those two keys are used to encrypt and authenticate the Tantivy files, using
/// AES-CTR mode for encryption and [HMAC-SHA256][hmac] for authentication. For
/// every encrypted file a new random 128 bit IV will be generated.
///
/// The MAC will be calculated only on the ciphertext:
///
/// ```text
///     mac = HMAC-SHA256(mac_key, ciphertext)
/// ```
///
/// The MAC will be at the end of the file and the ciphertext is between the IV
/// and the MAC:
///
/// ```text
///     file_data = (iv || ciphertext || mac)
/// ```
///
/// [aes]: https://en.wikipedia.org/wiki/Advanced_Encryption_Standard
/// [hkdf]: https://en.wikipedia.org/wiki/HKDF
/// [hmac]: https://en.wikipedia.org/wiki/HMAC
pub struct EncryptedMmapDirectory {
    mmap_dir: tantivy::directory::MmapDirectory,
    encryption_key: KeyBuffer,
    mac_key: KeyBuffer,
}

impl EncryptedMmapDirectory {
    /// Open an encrypted mmap directory, creating it if it doesn't exist.
    ///
    /// # Arguments
    ///
    /// - `path` - The path where the directory should reside in.
    /// - `key` - The high-entropy key used to encrypt the directory.
    ///
    /// Opening a directory with a different key than the one it was created
    /// with succeeds, but reading its files will then fail.
    pub fn open_or_create<P: AsRef<Path>>(path: P, key: &str) -> Result<Self, OpenDirectoryError> {
        let path = path.as_ref();

        if key.is_empty() {
            return Err(IoError::other("empty key").into_tv_err(path));
        }

        create_dir_all(path).map_err(|err| err.into_tv_err(path))?;

        // Expand the key into an encryption and MAC key.
        let mut hkdf_result = Zeroizing::new([0u8; KEY_SIZE * 2]);
        Hkdf::<Sha512>::new(None, key.as_bytes())
            .expand(HKDF_INFO, &mut *hkdf_result)
            .map_err(|e| IoError::other(format!("unable to expand the key: {e:?}")))
            .map_err(|err| err.into_tv_err(path))?;
        let (encryption_key, mac_key) = hkdf_result.split_at(KEY_SIZE);

        Ok(EncryptedMmapDirectory {
            mmap_dir: tantivy::directory::MmapDirectory::open(path)?,
            encryption_key: Zeroizing::new(encryption_key.to_vec()),
            mac_key: Zeroizing::new(mac_key.to_vec()),
        })
    }
}

/// An encrypted file, mapped in memory, whose bytes are decrypted when they're
/// read.
///
/// Only the bytes Tantivy asks for get decrypted, instead of the whole file
/// being kept decrypted in memory for as long as the index is open. AES-CTR
/// lets a read start anywhere in the file, by seeking the keystream.
///
/// The file's MAC is checked once, when it's opened: reads don't check it
/// again.
struct DecryptingFileHandle {
    /// The whole encrypted file: `iv || ciphertext || mac`.
    file: OwnedBytes,
    encryption_key: KeyBuffer,
}

impl fmt::Debug for DecryptingFileHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecryptingFileHandle").field("len", &self.len()).finish_non_exhaustive()
    }
}

impl HasLen for DecryptingFileHandle {
    fn len(&self) -> usize {
        self.file.len() - IV_SIZE - MAC_LENGTH
    }
}

impl FileHandle for DecryptingFileHandle {
    fn read_bytes(&self, range: Range<usize>) -> std::io::Result<OwnedBytes> {
        let (iv, ciphertext) = self.file.as_slice().split_at(IV_SIZE);
        let mut bytes = ciphertext[range.clone()].to_vec();

        let mut cipher = Aes256Ctr::new_from_slices(&self.encryption_key, iv)
            .map_err(|err| IoError::other(format!("couldn't initialize cipher: {err:?}")))?;
        cipher
            .try_seek(range.start as u64)
            .map_err(|err| IoError::other(format!("couldn't seek the keystream: {err:?}")))?;
        cipher.apply_keystream(&mut bytes);

        Ok(OwnedBytes::new(bytes))
    }
}

/// The [`Directory`] trait implementation for our [`EncryptedMmapDirectory`].
impl Directory for EncryptedMmapDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        self.mmap_dir.get_file_handle(path)
    }

    fn sync_directory(&self) -> std::io::Result<()> {
        self.mmap_dir.sync_directory()
    }

    fn open_read(&self, path: &Path) -> Result<FileSlice, OpenReadError> {
        let source = self.mmap_dir.open_read(path)?;

        // The file is mapped in memory, not read in.
        let file = source
            .read_bytes()
            .map_err(|err| <IoError as IntoTvError<OpenReadError>>::into_tv_err(err, path))?;

        // Creating the reader checks the MAC by streaming through the file, and
        // fails if the file is too short to hold an IV and a MAC.
        AesReader::<Aes256Ctr, _>::new::<HmacReset<Sha256>>(
            Cursor::new(file.as_slice()),
            &self.encryption_key,
            &self.mac_key,
            IV_SIZE,
            MAC_LENGTH,
        )
        .map_err(|err| <IoError as IntoTvError<OpenReadError>>::into_tv_err(err, path))?;

        Ok(FileSlice::new(Arc::new(DecryptingFileHandle {
            file,
            encryption_key: self.encryption_key.clone(),
        })))
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        self.mmap_dir.delete(path)
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        self.mmap_dir.exists(path)
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        let file = match self.mmap_dir.open_write(path)?.into_inner() {
            Ok(f) => f,
            Err(e) => {
                let error = IoError::from(e);
                return Err(error.into_tv_err(path));
            }
        };

        let writer = AesWriter::<Aes256Ctr, HmacReset<Sha256>, _>::new(
            file,
            &self.encryption_key,
            &self.mac_key,
            IV_SIZE,
        )
        .map_err(|err| err.into_tv_err(path))?;
        Ok(BufWriter::new(Box::new(writer)))
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        let data = self.mmap_dir.atomic_read(path)?;

        let mut reader = AesReader::<Aes256Ctr, _>::new::<HmacReset<Sha256>>(
            Cursor::new(data),
            &self.encryption_key,
            &self.mac_key,
            IV_SIZE,
            MAC_LENGTH,
        )
        .map_err(|err| <IoError as IntoTvError<OpenReadError>>::into_tv_err(err, path))?;
        let mut decrypted = Vec::new();

        reader
            .read_to_end(&mut decrypted)
            .map_err(|err| <IoError as IntoTvError<OpenReadError>>::into_tv_err(err, path))?;
        Ok(decrypted)
    }

    fn atomic_write(&self, path: &Path, data: &[u8]) -> std::io::Result<()> {
        let mut encrypted = Vec::new();
        {
            let mut writer = AesWriter::<Aes256Ctr, HmacReset<Sha256>, _>::new(
                &mut encrypted,
                &self.encryption_key,
                &self.mac_key,
                IV_SIZE,
            )?;
            writer.write_all(data)?;
        }

        self.mmap_dir.atomic_write(path, &encrypted)
    }

    fn watch(&self, watch_callback: WatchCallback) -> Result<WatchHandle, tantivy::TantivyError> {
        self.mmap_dir.watch(watch_callback)
    }

    fn acquire_lock(&self, lock: &Lock) -> Result<DirectoryLock, LockError> {
        // The lock files aren't encrypted, this is fine since they won't
        // contain any data. They will be an empty file and a lock will be
        // placed on them using e.g. flock(2) on macOS and Linux.
        self.mmap_dir.acquire_lock(lock)
    }
}

// This Tantivy trait is used to indicate when no more writes are expected to be
// done on a writer.
impl<
    E: StreamCipher + KeyIvInit + Sync + Send,
    M: Mac + KeyInit + Update + FixedOutputReset + Sync + Send,
    W: Write + Sync + Send,
> TerminatingWrite for AesWriter<E, M, W>
{
    fn terminate_ref(&mut self, _: AntiCallToken) -> std::io::Result<()> {
        self.finalize()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tantivy::{HasLen, directory::Directory};
    use tempfile::tempdir;

    use crate::encrypted::encrypted_dir::EncryptedMmapDirectory;

    #[test]
    fn create_new_store_and_reopen() {
        let tmpdir = tempdir().unwrap();
        let file = Path::new("file");

        let dir = EncryptedMmapDirectory::open_or_create(tmpdir.path(), "key")
            .expect("Can't create a new store");
        dir.atomic_write(file, b"data").unwrap();
        drop(dir);

        let dir = EncryptedMmapDirectory::open_or_create(tmpdir.path(), "key")
            .expect("Can't open the existing store");
        assert_eq!(dir.atomic_read(file).unwrap(), b"data");
        drop(dir);

        let dir = EncryptedMmapDirectory::open_or_create(tmpdir.path(), "another key").unwrap();
        assert!(dir.atomic_read(file).is_err(), "Read a file with the wrong key");
    }

    #[test]
    fn open_read_decrypts_any_range() {
        let tmpdir = tempdir().unwrap();
        let file = Path::new("file");
        let data = (0..100_000).map(|i| (i * 7 % 251) as u8).collect::<Vec<_>>();

        let dir = EncryptedMmapDirectory::open_or_create(tmpdir.path(), "key").unwrap();
        dir.atomic_write(file, &data).unwrap();

        let slice = dir.open_read(file).unwrap();
        assert_eq!(slice.len(), data.len());
        assert_eq!(slice.read_bytes().unwrap().as_slice(), &data[..]);

        // Ranges starting and ending inside and across 16 bytes AES blocks.
        for range in [0..0, 0..1, 3..17, 15..16, 16..48, 17..4_097, 50_001..99_999, 99_990..100_000]
        {
            assert_eq!(
                slice.read_bytes_slice(range.clone()).unwrap().as_slice(),
                &data[range.clone()],
                "range {range:?}"
            );
        }
    }

    #[test]
    fn open_read_checks_the_mac() {
        let tmpdir = tempdir().unwrap();
        let file = Path::new("file");

        let dir = EncryptedMmapDirectory::open_or_create(tmpdir.path(), "key").unwrap();
        dir.atomic_write(file, b"some data to tamper with").unwrap();

        // A wrong key fails the MAC check.
        let other_dir =
            EncryptedMmapDirectory::open_or_create(tmpdir.path(), "another key").unwrap();
        assert!(other_dir.open_read(file).is_err(), "Read a file with the wrong key");

        // So does a modified file.
        let path = tmpdir.path().join(file);
        let mut encrypted = std::fs::read(&path).unwrap();
        encrypted[20] ^= 1;
        std::fs::write(&path, encrypted).unwrap();

        let dir = EncryptedMmapDirectory::open_or_create(tmpdir.path(), "key").unwrap();
        assert!(dir.open_read(file).is_err(), "Read a modified file");
    }

    #[test]
    fn create_store_with_empty_key() {
        let tmpdir = tempdir().unwrap();
        let dir = EncryptedMmapDirectory::open_or_create(tmpdir.path(), "");
        assert!(dir.is_err(), "Created a store with an empty key");
    }

    #[test]
    fn create_store_in_nonexistent_directory() {
        let tmpdir = tempdir().unwrap();
        let nested_path = tmpdir.path().join("nested").join("directory");
        let dir = EncryptedMmapDirectory::open_or_create(&nested_path, "key")
            .expect("Should create store in non-existent nested directory");
        drop(dir);
    }
}
