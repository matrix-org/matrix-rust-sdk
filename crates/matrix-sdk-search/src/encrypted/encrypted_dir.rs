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
    fs::{File, create_dir_all},
    io::{BufWriter, Cursor, Error as IoError, ErrorKind, Read, Write},
    path::Path,
    sync::Arc,
};

use aes::{
    Aes256,
    cipher::{InvalidLength, KeyInit, KeyIvInit, StreamCipher},
};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use pbkdf2::pbkdf2;
use rand::{Rng, thread_rng};
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

/// Key derivation result type for our initial key derivation. Consists of a
/// tuple containing a encryption key, a MAC key, and a random salt.
type InitialKeyDerivationResult = (KeyBuffer, KeyBuffer, Vec<u8>);

/// Key derivation result for our subsequent key derivations. The salt will be
/// read from our key file and we will re-derive our encryption and MAC keys.
type KeyDerivationResult = (KeyBuffer, KeyBuffer);

// The constants here are chosen to be similar to the constants for the Matrix
// key export format[1].
// [1] https://spec.matrix.org/v1.15/client-server-api/#key-export-format
const KEYFILE: &str = "seshat-index.key";
// 16 byte random salt.
const SALT_SIZE: usize = 16;
// 16 byte random IV for the AES-CTR mode.
const IV_SIZE: usize = 16;
// 32 byte or 256 bit encryption keys.
const KEY_SIZE: usize = 32;
// 32 byte message authentication code since HMAC-SHA256 is used.
const MAC_LENGTH: usize = 32;
// 1 byte for the store version.
const VERSION: u8 = 1;

#[cfg(test)]
// Tests don't need to protect against brute force attacks.
pub(crate) const PBKDF_COUNT: u32 = 10;

#[cfg(not(test))]
// This is quite a bit lower than the spec since the encrypted key and index
// will not be public. An attacker would need to get hold of the encrypted
// index, its key and only then move on to bruteforcing the key. Even if the
// attacker decrypts the index he wouldn't get to the events itself, only to the
// index of them.
pub(crate) const PBKDF_COUNT: u32 = 10_000;

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
/// When a new EncryptedMmapDirectory is created a random 256 bit AES key is
/// generated, the store key. This key is encrypted using a key that will be
/// derived with the user provided passphrase using [PBKDF2][pbkdf]. We generate
/// a random 128 bit salt and derive a 512 bit key using PBKDF2:
///
/// ```text
///     derived_key = PBKDF2(SHA512, passphrase, salt, count, 512)
/// ```
///
/// After key derivation, the key is split into a 256 bit AES encryption key and
/// a 256 bit MAC key.
///
/// The store key will be encrypted using AES-CTR with the encryption key that
/// was derived before, a random IV will be generated before encrypting the
/// store_key:
///
/// ```text
///     ciphertext = AES256-CTR(iv, store_key)
/// ```
///
/// A MAC of the encrypted ciphertext will be created using
/// the derived MAC key and [HMAC-SHA256][hmac]:
///
/// ```text
///     mac = HMAC-SHA256(mac_key, version || iv || salt || ciphertext)
/// ```
///
/// The store key will be written to a file concatenated with a store version,
/// IV, salt, PBKDF count, and MAC. The PBKDF count will be stored using the big
/// endian byte order:
///
/// ```text
///     key_file = (version || iv || salt || pbkdf_count || mac || key_ciphertext)
/// ```
///
/// Our store key will be used to encrypt the many files that Tantivy generates.
/// For this, the store key will be expanded into an 256 bit encryption key and
/// a 256 bit MAC key using [HKDF][hkdf].
///
/// ```text
///     encryption_key, mac_key = HKDF(SHA512, store_key, "", 512)
/// ```
///
/// Those two keys are used to encrypt and authenticate the Tantivy files. The
/// encryption scheme is similar to the one used for the store key, AES-CTR mode
/// for encryption and HMAC-SHA256 for authentication. For every encrypted file
/// a new random 128 bit IV will be generated.
///
/// The MAC will be calculated only on the ciphertext:
///
/// ```text
///     mac = HMAC-SHA256(mac_key, ciphertext)
/// ```
///
/// The file format differs a bit, the MAC will be at the end of the file and
/// the ciphertext is between the IV and salt:
///
/// ```text
///     file_data = (iv || ciphertext || mac)
/// ```
///
/// [aes]: https://en.wikipedia.org/wiki/Advanced_Encryption_Standard
/// [pbkdf]: https://en.wikipedia.org/wiki/PBKDF2
/// [hkdf]: https://en.wikipedia.org/wiki/HKDF
/// [hmac]: https://en.wikipedia.org/wiki/HMAC
pub struct EncryptedMmapDirectory {
    mmap_dir: tantivy::directory::MmapDirectory,
    encryption_key: KeyBuffer,
    mac_key: KeyBuffer,
}

impl EncryptedMmapDirectory {
    fn new(store_key: KeyBuffer, path: &Path) -> Result<Self, OpenDirectoryError> {
        // Expand the store key into a encryption and MAC key.
        let (encryption_key, mac_key) = EncryptedMmapDirectory::expand_store_key(&store_key)
            .map_err(|err| err.into_tv_err(path))?;

        // Open our underlying bare Tantivy mmap based directory.
        let mmap_dir = tantivy::directory::MmapDirectory::open(path)?;

        Ok(EncryptedMmapDirectory { mmap_dir, encryption_key, mac_key })
    }
    /// Open a encrypted mmap directory. If the directory is empty a new
    ///   directory key will be generated and encrypted with the given
    /// passphrase.
    ///
    /// If a new store is created, this method will randomly generated a new
    ///   store key and encrypted using the given passphrase.
    ///
    /// # Arguments
    ///
    /// * `path` - The path where the directory should reside in.
    /// * `passphrase` - The passphrase that was used to encrypt our directory
    ///   or the one that will be used to encrypt our directory.
    /// * `key_derivation_count` - The number of iterations that our key
    ///   derivation function should use. Can't be lower than 1, should be
    ///   chosen as high as possible, depending on how much time is acceptable
    ///   for the caller to wait. Is only used when a new store is created. The
    ///   count will be stored with the store key.
    ///
    /// Returns an error if the path does not exist, if it is not a directory or
    ///   if there was an error when trying to decrypt the directory key e.g.
    /// the   given passphrase was incorrect.
    pub fn open_or_create<P: AsRef<Path>>(
        path: P,
        passphrase: &str,
        key_derivation_count: u32,
    ) -> Result<Self, OpenDirectoryError> {
        if passphrase.is_empty() {
            return Err(IoError::other("empty passphrase").into_tv_err(path.as_ref()));
        }

        if key_derivation_count == 0 {
            return Err(IoError::other("invalid key derivation count").into_tv_err(path.as_ref()));
        }

        let key_path = path.as_ref().join(KEYFILE);
        let key_file = File::open(&key_path);

        // Either load a store key or create a new store key if the key file
        // doesn't exist.
        let store_key = match key_file {
            Ok(k) => {
                let (_, key) = EncryptedMmapDirectory::load_store_key(k, passphrase)
                    .map_err(|err| err.into_tv_err(path.as_ref()))?;
                key
            }
            Err(e) => {
                if e.kind() != ErrorKind::NotFound {
                    return Err(e.into_tv_err(path.as_ref()));
                }
                EncryptedMmapDirectory::create_new_store(
                    &key_path,
                    passphrase,
                    key_derivation_count,
                )?
            }
        };
        EncryptedMmapDirectory::new(store_key, path.as_ref())
    }

    /// Open a encrypted mmap directory.
    ///
    /// # Arguments
    ///
    /// * `path` - The path where the directory should reside in.
    /// * `passphrase` - The passphrase that was used to encrypt our directory.
    ///
    /// Returns an error if the path does not exist, if it is not a directory or
    ///   if there was an error when trying to decrypt the directory key e.g.
    /// the   given passphrase was incorrect.
    // This isn't currently used anywhere, but it will make sense if the
    // EncryptedMmapDirectory gets upstreamed.
    #[allow(dead_code)]
    pub fn open<P: AsRef<Path>>(path: P, passphrase: &str) -> Result<Self, OpenDirectoryError> {
        if passphrase.is_empty() {
            return Err(IoError::other("empty passphrase").into_tv_err(path.as_ref()));
        }

        let key_path = path.as_ref().join(KEYFILE);
        let key_file = File::open(key_path).map_err(|err| err.into_tv_err(path.as_ref()))?;

        // Expand the store key into a encryption and MAC key.
        let (_, store_key) = EncryptedMmapDirectory::load_store_key(key_file, passphrase)
            .map_err(|err| err.into_tv_err(path.as_ref()))?;
        EncryptedMmapDirectory::new(store_key, path.as_ref())
    }

    /// Change the passphrase that is used to encrypt the store key.
    /// This will decrypt and re-encrypt the store key using the new passphrase.
    ///
    /// # Arguments
    ///
    /// * `path` - The path where the directory resides in.
    /// * `old_passphrase` - The currently used passphrase.
    /// * `new_passphrase` - The passphrase that should be used from now on.
    /// * `new_key_derivation_count` - The key derivation count that should be
    ///   used for the re-encrypted store key.
    #[allow(dead_code)] // already implemented and will almost certainly be used.
    pub fn change_passphrase<P: AsRef<Path>>(
        path: P,
        old_passphrase: &str,
        new_passphrase: &str,
        new_key_derivation_count: u32,
    ) -> Result<(), OpenDirectoryError> {
        if old_passphrase.is_empty() || new_passphrase.is_empty() {
            return Err(IoError::other("empty passphrase").into_tv_err(path.as_ref()));
        }
        if new_key_derivation_count == 0 {
            return Err(IoError::other("invalid key derivation count").into_tv_err(path.as_ref()));
        }

        let key_path = path.as_ref().join(KEYFILE);
        let key_file = File::open(&key_path).map_err(|err| err.into_tv_err(path.as_ref()))?;

        // Load our store key using the old passphrase.
        let (_, store_key) = EncryptedMmapDirectory::load_store_key(key_file, old_passphrase)
            .map_err(|err| err.into_tv_err(path.as_ref()))?;
        // Derive new encryption keys using the new passphrase.
        let (key, hmac_key, salt) =
            EncryptedMmapDirectory::derive_key(new_passphrase, new_key_derivation_count)
                .map_err(|err| err.into_tv_err(path.as_ref()))?;
        // Re-encrypt our store key using the newly derived keys.
        EncryptedMmapDirectory::encrypt_store_key(
            &key,
            &salt,
            new_key_derivation_count,
            &hmac_key,
            &store_key,
            &key_path,
        )
        .map_err(|err| err.into_tv_err(path.as_ref()))?;

        Ok(())
    }

    /// Expand the given store key into an encryption key and HMAC key.
    fn expand_store_key(store_key: &[u8]) -> std::io::Result<KeyDerivationResult> {
        let mut hkdf_result = Zeroizing::new([0u8; KEY_SIZE * 2]);

        let hkdf = Hkdf::<Sha512>::new(None, store_key);
        hkdf.expand(&[], &mut *hkdf_result)
            .map_err(|e| IoError::other(format!("unable to expand store key: {:?}", e)))?;
        let (key, hmac_key) = hkdf_result.split_at(KEY_SIZE);
        Ok((Zeroizing::new(Vec::from(key)), Zeroizing::new(Vec::from(hmac_key))))
    }

    /// Load a store key from the given file and decrypt it using the given
    /// passphrase.
    fn load_store_key(mut key_file: File, passphrase: &str) -> Result<(u32, KeyBuffer), IoError> {
        let mut iv = [0u8; IV_SIZE];
        let mut salt = [0u8; SALT_SIZE];
        let mut expected_mac = [0u8; MAC_LENGTH];
        let mut version = [0u8; 1];
        let mut encrypted_key = vec![];

        // Read our iv, salt, mac, and encrypted key from our key file.
        key_file.read_exact(&mut version)?;
        key_file.read_exact(&mut iv)?;
        key_file.read_exact(&mut salt)?;
        let pbkdf_count = key_file.read_u32::<BigEndian>()?;
        key_file.read_exact(&mut expected_mac)?;

        // Our key will be AES encrypted in CTR mode meaning the ciphertext
        // will have the same size as the plaintext. Read at most KEY_SIZE
        // bytes here so we don't end up filling up memory unnecessarily if
        // someone modifies the file.
        key_file.take(KEY_SIZE as u64).read_to_end(&mut encrypted_key)?;

        if version[0] != VERSION {
            return Err(IoError::other("invalid index store version"));
        }

        // Re-derive our key using the passphrase and salt.
        let (key, hmac_key) = EncryptedMmapDirectory::rederive_key(passphrase, &salt, pbkdf_count)
            .map_err(|e| IoError::other(format!("error deriving key: {:?}", e)))?;

        // First check our MAC of the encrypted key.
        let mac = EncryptedMmapDirectory::calculate_hmac(
            version[0],
            &iv,
            &salt,
            &encrypted_key,
            &hmac_key,
        )?;

        if mac.verify_slice(&expected_mac).is_err() {
            return Err(IoError::other("invalid MAC of the store key"));
        }

        let mut decryptor = Aes256Ctr::new_from_slices(&key, &iv)
            .map_err(|e| IoError::other(format!("error initializing cipher {:?}", e)))?;

        let mut out = Zeroizing::new(encrypted_key);
        decryptor
            .try_apply_keystream(&mut out)
            .map_err(|_| IoError::other("Decryption error, reached end of the keystream."))?;

        Ok((pbkdf_count, out))
    }

    /// Calculate a HMAC for the given inputs.
    fn calculate_hmac(
        version: u8,
        iv: &[u8],
        salt: &[u8],
        encrypted_data: &[u8],
        hmac_key: &[u8],
    ) -> std::io::Result<Hmac<Sha256>> {
        let mut hmac = <Hmac<Sha256> as Mac>::new_from_slice(hmac_key)
            .map_err(|e| IoError::other(format!("error creating hmac: {:?}", e)))?;
        <Hmac<Sha256> as Mac>::update(&mut hmac, &[version]);
        <Hmac<Sha256> as Mac>::update(&mut hmac, iv);
        <Hmac<Sha256> as Mac>::update(&mut hmac, salt);
        <Hmac<Sha256> as Mac>::update(&mut hmac, encrypted_data);
        Ok(hmac)
    }

    /// Create a new store key, encrypt it with the given passphrase and store
    /// it in the given path.
    fn create_new_store(
        key_path: &Path,
        passphrase: &str,
        pbkdf_count: u32,
    ) -> Result<KeyBuffer, OpenDirectoryError> {
        let dir_path = key_path.parent().unwrap_or(key_path);

        create_dir_all(dir_path).map_err(|err| err.into_tv_err(dir_path))?;
        // Derive a AES key from our passphrase using a randomly generated salt
        // to prevent bruteforce attempts using rainbow tables.
        let (key, hmac_key, salt) = EncryptedMmapDirectory::derive_key(passphrase, pbkdf_count)
            .map_err(|err| err.into_tv_err(dir_path))?;
        // Generate a new random store key. This key will encrypt our Tantivy
        // indexing files. The key itself is stored encrypted using the derived
        // key.
        let store_key =
            EncryptedMmapDirectory::generate_key().map_err(|err| err.into_tv_err(key_path))?;

        // Encrypt and save the encrypted store key to a file.
        EncryptedMmapDirectory::encrypt_store_key(
            &key,
            &salt,
            pbkdf_count,
            &hmac_key,
            &store_key,
            key_path,
        )
        .map_err(|err| err.into_tv_err(key_path))?;

        Ok(store_key)
    }

    /// Encrypt the given store key and save it in the given path.
    fn encrypt_store_key(
        key: &[u8],
        salt: &[u8],
        pbkdf_count: u32,
        hmac_key: &[u8],
        store_key: &[u8],
        key_path: &Path,
    ) -> Result<(), IoError> {
        // Generate a random initialization vector for our AES encryptor.
        let iv = EncryptedMmapDirectory::generate_iv()?;
        let mut encryptor = Aes256Ctr::new_from_slices(key, &iv)
            .map_err(|e| IoError::other(format!("error initializing cipher: {:?}", e)))?;

        let mut encrypted_key = [0u8; KEY_SIZE];
        encrypted_key.copy_from_slice(store_key);

        let mut key_file = File::create(key_path)?;

        // Write down our public salt and iv first, those will be needed to
        // decrypt the key again.
        key_file.write_all(&[VERSION])?;
        key_file.write_all(&iv)?;
        key_file.write_all(salt)?;
        key_file.write_u32::<BigEndian>(pbkdf_count)?;

        // Encrypt our key.
        encryptor
            .try_apply_keystream(&mut encrypted_key)
            .map_err(|e| IoError::other(format!("unable to encrypt store key: {:?}", e)))?;

        // Calculate a MAC for our encrypted key and store it in the file before
        // the key.
        let mac =
            EncryptedMmapDirectory::calculate_hmac(VERSION, &iv, salt, &encrypted_key, hmac_key)?;
        let mac = mac.finalize();
        let mac = mac.into_bytes();
        key_file.write_all(mac.as_slice())?;

        // Write down the encrypted key.
        key_file.write_all(&encrypted_key)?;

        Ok(())
    }

    /// Generate a random IV.
    fn generate_iv() -> Result<[u8; IV_SIZE], IoError> {
        let mut iv = [0u8; IV_SIZE];
        let mut rng = thread_rng();
        rng.try_fill(&mut iv[..])
            .map_err(|e| IoError::other(format!("error generating iv: {:?}", e)))?;
        Ok(iv)
    }

    /// Generate a random key.
    fn generate_key() -> Result<KeyBuffer, IoError> {
        let mut key = Zeroizing::new(vec![0u8; KEY_SIZE]);
        let mut rng = thread_rng();
        rng.try_fill(&mut key[..])
            .map_err(|e| IoError::other(format!("error generating key: {:?}", e)))?;
        Ok(key)
    }

    /// Derive two keys from the given passphrase and the given salt using
    /// PBKDF2.
    fn rederive_key(
        passphrase: &str,
        salt: &[u8],
        pbkdf_count: u32,
    ) -> Result<KeyDerivationResult, InvalidLength> {
        let mut pbkdf_result = Zeroizing::new([0u8; KEY_SIZE * 2]);

        pbkdf2::<Hmac<Sha512>>(passphrase.as_bytes(), salt, pbkdf_count, &mut *pbkdf_result)?;
        let (key, hmac_key) = pbkdf_result.split_at(KEY_SIZE);
        Ok((Zeroizing::new(Vec::from(key)), Zeroizing::new(Vec::from(hmac_key))))
    }

    /// Generate a random salt and derive two keys from the salt and the given
    /// passphrase.
    fn derive_key(
        passphrase: &str,
        pbkdf_count: u32,
    ) -> Result<InitialKeyDerivationResult, IoError> {
        let mut rng = thread_rng();
        let mut salt = vec![0u8; SALT_SIZE];
        rng.try_fill(&mut salt[..])
            .map_err(|e| IoError::other(format!("error generating salt: {:?}", e)))?;

        let (key, hmac_key) = EncryptedMmapDirectory::rederive_key(passphrase, &salt, pbkdf_count)
            .map_err(|e| IoError::other(format!("error deriving key: {:?}", e)))?;
        Ok((key, hmac_key, salt))
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

        let mut reader = AesReader::<Aes256Ctr, _>::new::<Hmac<Sha256>>(
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

        let writer = AesWriter::<Aes256Ctr, Hmac<Sha256>, _>::new(
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

        let mut reader = AesReader::<Aes256Ctr, _>::new::<Hmac<Sha256>>(
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
            let mut writer = AesWriter::<Aes256Ctr, Hmac<Sha256>, _>::new(
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
    use tempfile::tempdir;

    use crate::encrypted::encrypted_dir::{EncryptedMmapDirectory, PBKDF_COUNT};

    #[test]
    fn create_new_store_and_reopen() {
        let tmpdir = tempdir().unwrap();
        let dir = EncryptedMmapDirectory::open_or_create(tmpdir.path(), "wordpass", PBKDF_COUNT)
            .expect("Can't create a new store");
        drop(dir);
        let dir = EncryptedMmapDirectory::open(tmpdir.path(), "wordpass")
            .expect("Can't open the existing store");
        drop(dir);
        let dir = EncryptedMmapDirectory::open(tmpdir.path(), "password");
        assert!(dir.is_err(), "Opened an existing store with the wrong passphrase");
    }

    #[test]
    fn create_store_with_empty_passphrase() {
        let tmpdir = tempdir().unwrap();
        let dir = EncryptedMmapDirectory::open(tmpdir.path(), "");
        assert!(dir.is_err(), "Opened an existing store with the wrong passphrase");
    }

    #[test]
    fn change_passphrase() {
        let tmpdir = tempdir().unwrap();
        let dir = EncryptedMmapDirectory::open_or_create(tmpdir.path(), "wordpass", PBKDF_COUNT)
            .expect("Can't create a new store");

        drop(dir);
        EncryptedMmapDirectory::change_passphrase(
            tmpdir.path(),
            "wordpass",
            "password",
            PBKDF_COUNT,
        )
        .expect("Can't change passphrase");
        let dir = EncryptedMmapDirectory::open(tmpdir.path(), "wordpass");
        assert!(dir.is_err(), "Opened an existing store with the old passphrase");
        let _ = EncryptedMmapDirectory::open(tmpdir.path(), "password")
            .expect("Can't open the store with the new passphrase");
    }

    #[test]
    fn create_store_in_nonexistent_directory() {
        let tmpdir = tempdir().unwrap();
        let nested_path = tmpdir.path().join("nested").join("directory");
        let dir = EncryptedMmapDirectory::open_or_create(&nested_path, "password", PBKDF_COUNT)
            .expect("Should create store in non-existent nested directory");
        drop(dir);
    }
}
