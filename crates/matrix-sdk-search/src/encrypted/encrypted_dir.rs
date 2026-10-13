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
    fs::create_dir_all,
    io::{BufWriter, Cursor, Error as IoError, Read, Write},
    path::Path,
    sync::Arc,
};

use aes::{
    Aes256,
    cipher::{KeyInit, KeyIvInit, StreamCipher},
};
use hkdf::Hkdf;
use hmac::{HmacReset, Mac};
use sha2::{
    Sha256, Sha512,
    digest::{FixedOutputReset, Update},
};
use tantivy::directory::{
    AntiCallToken, Directory, DirectoryLock, FileHandle, FileSlice, Lock, TerminatingWrite,
    WatchCallback, WatchHandle, WritePtr,
    error::{DeleteError, LockError, OpenDirectoryError, OpenReadError, OpenWriteError},
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
///     encryption_key, mac_key = HKDF(SHA512, key, "", 512)
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
            .expand(&[], &mut *hkdf_result)
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

        let bytes = source
            .read_bytes()
            .map_err(|err| <IoError as IntoTvError<OpenReadError>>::into_tv_err(err, path))?;

        let mut reader = AesReader::<Aes256Ctr, _>::new::<HmacReset<Sha256>>(
            Cursor::new(bytes.as_slice()),
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

        Ok(FileSlice::from(decrypted))
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

    use tantivy::directory::Directory;
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
