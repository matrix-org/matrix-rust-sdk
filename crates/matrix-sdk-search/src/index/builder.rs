use std::{fs, io, ops::Deref, path::PathBuf, sync::Arc};

use ruma::OwnedRoomId;
use tantivy::{
    Index,
    directory::{MmapDirectory, error::OpenDirectoryError},
};
use zeroize::Zeroizing;

use crate::{
    encrypted::encrypted_dir::{EncryptedMmapDirectory, PBKDF_COUNT},
    error::IndexError,
    index::RoomIndex,
    schema::{MatrixSearchIndexSchema, RoomMessageSchema},
};

/// Builder for [`RoomIndex`].
pub struct RoomIndexBuilder {}

impl RoomIndexBuilder {
    /// Make an index on disk
    pub fn new_on_disk<R: Into<OwnedRoomId>>(
        path: PathBuf,
        room_id: R,
    ) -> PhysicalRoomIndexBuilder {
        PhysicalRoomIndexBuilder::new(path, room_id.into())
    }

    /// Make an index in memory
    pub fn new_in_memory<R: Into<OwnedRoomId>>(room_id: R) -> MemoryRoomIndexBuilder {
        MemoryRoomIndexBuilder::new(room_id.into())
    }
}

/// Incomplete builder for [`RoomIndex`] on disk.
pub struct PhysicalRoomIndexBuilder {
    path: PathBuf,
    room_id: OwnedRoomId,
}

impl PhysicalRoomIndexBuilder {
    /// Make an new [`PhysicalRoomIndexBuilder`]
    pub(crate) fn new(path: PathBuf, room_id: OwnedRoomId) -> PhysicalRoomIndexBuilder {
        PhysicalRoomIndexBuilder { path, room_id }
    }

    /// Make an unencrypted index
    pub fn unencrypted(&self) -> UnencryptedPhysicalRoomIndexBuilder {
        UnencryptedPhysicalRoomIndexBuilder {
            path: self.path.clone(),
            room_id: self.room_id.clone(),
        }
    }

    /// Make an index encrypted using a passphrase.
    pub fn encrypted_with_passphrase<P: Into<String>>(
        &self,
        password: P,
    ) -> EncryptedPhysicalRoomIndexBuilder {
        EncryptedPhysicalRoomIndexBuilder {
            path: self.path.clone(),
            room_id: self.room_id.clone(),
            password: Some(Zeroizing::new(password.into())),
            key: None,
        }
    }

    /// Make an index encrypted using a key.
    pub fn encrypted_with_key(&self, key: [u8; 32]) -> EncryptedPhysicalRoomIndexBuilder {
        EncryptedPhysicalRoomIndexBuilder {
            path: self.path.clone(),
            room_id: self.room_id.clone(),
            password: None,
            key: Some(Zeroizing::new(key)),
        }
    }
}

/// Complete builder for [`RoomIndex`] on disk.
pub struct UnencryptedPhysicalRoomIndexBuilder {
    path: PathBuf,
    room_id: OwnedRoomId,
}

impl UnencryptedPhysicalRoomIndexBuilder {
    /// Build the [`RoomIndex`]
    pub fn build(&self) -> Result<RoomIndex, IndexError> {
        let path = self.path.join(self.room_id.as_str());
        let mmap_dir = match MmapDirectory::open(path) {
            Ok(dir) => Ok(dir),
            Err(err) => match err {
                OpenDirectoryError::DoesNotExist(path) => {
                    fs::create_dir_all(path.clone()).map_err(|err| {
                        OpenDirectoryError::IoError {
                            io_error: Arc::new(err),
                            directory_path: path.to_path_buf(),
                        }
                    })?;
                    MmapDirectory::open(path)
                }
                _ => Err(err),
            },
        }?;
        let schema = RoomMessageSchema::new();
        let index = Index::open_or_create(mmap_dir, schema.as_tantivy_schema())?;
        Ok(RoomIndex::new_with(index, schema, &self.room_id))
    }
}

/// Complete builder for [`RoomIndex`] on disk.
pub struct EncryptedPhysicalRoomIndexBuilder {
    path: PathBuf,
    room_id: OwnedRoomId,
    password: Option<Zeroizing<String>>,
    key: Option<Zeroizing<[u8; 32]>>,
}

impl EncryptedPhysicalRoomIndexBuilder {
    /// Build the [`RoomIndex`]
    pub fn build(&self) -> Result<RoomIndex, IndexError> {
        let path = self.path.join(self.room_id.as_str());
        let mmap_dir = match self.open_or_create(&path) {
            Ok(dir) => Ok(dir),
            Err(err) => match err {
                OpenDirectoryError::DoesNotExist(path) => {
                    fs::create_dir_all(path.clone()).map_err(|err| {
                        OpenDirectoryError::IoError {
                            io_error: Arc::new(err),
                            directory_path: path.to_path_buf(),
                        }
                    })?;

                    self.open_or_create(&path)
                }
                _ => Err(err),
            },
        }?;
        let schema = RoomMessageSchema::new();
        let index = Index::open_or_create(mmap_dir, schema.as_tantivy_schema())?;
        Ok(RoomIndex::new_with(index, schema, &self.room_id))
    }

    fn open_or_create(&self, path: &PathBuf) -> Result<EncryptedMmapDirectory, OpenDirectoryError> {
        if let Some(key) = &self.key {
            EncryptedMmapDirectory::open_or_create_with_key(path, key.deref())
        } else if let Some(password) = &self.password {
            EncryptedMmapDirectory::open_or_create_with_passphrase(path, password, PBKDF_COUNT)
        } else {
            Err(OpenDirectoryError::FailedToCreateTempDir(Arc::new(io::Error::new(
                io::ErrorKind::InvalidInput,
                IndexError::InvalidEncryptionConfig,
            ))))
        }
    }
}

/// Builder for [`RoomIndex`] in memory
pub struct MemoryRoomIndexBuilder {
    room_id: OwnedRoomId,
}

impl MemoryRoomIndexBuilder {
    /// Make an new [`MemoryIndexBuilder`]
    pub(crate) fn new(room_id: OwnedRoomId) -> MemoryRoomIndexBuilder {
        MemoryRoomIndexBuilder { room_id }
    }

    /// Build the [`RoomIndex`]
    pub fn build(&self) -> RoomIndex {
        let schema = RoomMessageSchema::new();
        let index = Index::create_in_ram(schema.as_tantivy_schema());
        RoomIndex::new_with(index, schema, &self.room_id)
    }
}
