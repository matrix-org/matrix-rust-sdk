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

//! An implementation of `deadpool::managed::Manager` for `rusqlite`
//! for usage in WASM environments.
//!
//! Similar to the one implemented in `crate::connection::default`,
//! we do not implement connection recycling here. Mostly due to
//! [`managed::Manager::recycle`] method expecting a future with `Send`
//! bound which is not available in WASM environment.

use std::{
    cell::RefCell,
    convert::Infallible,
    ops::DerefMut,
    path::{Path, PathBuf},
};

use deadpool::managed::{self, Metrics};
use rusqlite::OpenFlags;
use sqlite_wasm_vfs::sahpool::{OpfsSAHPoolCfgBuilder, OpfsSAHPoolUtil, install};

use crate::OpenStoreError;

/// [`Manager`][managed::Manager] for creating and recycling SQLite
/// [`Connection`]s.
#[derive(Debug)]
pub struct Manager {
    database_path: PathBuf,

    /// VFS used by this database connection in WASM environment.
    vfs: String,
}

impl Manager {
    /// Creates a new [`Manager`] for a database.
    pub async fn new(path: &Path, database_name: &str) -> Result<Self, OpenStoreError> {
        setup_vfs(path).await?;

        // We don't need full path for database path as the parent
        // directories are managed by VFS.
        let database_path = PathBuf::from(database_name);

        Ok(Self { database_path, vfs: get_vfs_name(path) })
    }
}

impl managed::Manager for Manager {
    type Type = ConnectionWrapper<rusqlite::Connection>;
    type Error = rusqlite::Error;

    async fn create(&self) -> Result<Self::Type, Self::Error> {
        let path = self.database_path.clone();

        let conn = rusqlite::Connection::open_with_flags_and_vfs(
            path,
            OpenFlags::default(),
            self.vfs.as_str(),
        )?;
        Ok(ConnectionWrapper::new(conn))
    }

    async fn recycle(
        &self,
        _conn: &mut Self::Type,
        _: &Metrics,
    ) -> managed::RecycleResult<Self::Error> {
        // We cannot implement connection recycling
        // at the moment, due to
        // `managed::Manager::recycle` expecting
        // a future with `Send` bound which is not
        // available in WASM environments.
        Ok(())
    }
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[derive(Debug)]
/// Wrapper object for providing interior mutability similar to [`SyncWrapper`]
/// without `Send` requirement.
///
/// Like [`SyncWrapper`], access to the wrapped object is provided via the
/// [`ConnectionWrapper::interact()`] method.
///
/// [`SyncWrapper`]: deadpool_sync::SyncWrapper
pub struct ConnectionWrapper<T>(RefCell<T>);

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl<T> ConnectionWrapper<T> {
    /// Creates a new wrapped object.
    pub fn new(value: T) -> Self {
        Self(RefCell::new(value))
    }

    /// Interacts with the underlying object.
    ///
    /// Expects a closure that takes the object as its parameter.
    pub async fn interact<F, R>(&self, f: F) -> Result<R, Infallible>
    where
        F: FnOnce(&mut T) -> R,
    {
        // This async block here is to maintain API compatibility with `SyncWrapper`
        // without triggering clippy warning for `async fn` without a call to
        // `await` inside
        async {
            let mut value = self.0.borrow_mut();

            Ok(f(value.deref_mut()))
        }
        .await
    }
}

/// Configure VFS name using provided path.
pub fn get_vfs_name(path: &Path) -> String {
    format!(
        "matrix-opfs-sahpool+{}",
        uri_encode::encode_uri_component(path.to_string_lossy().as_ref())
    )
}

/// Setup VFS for SQLite database using provided path and return management
/// tool.
///
/// Subsequence call to this function will simply return the management tool
/// without installing vfs.
pub async fn setup_vfs(path: &Path) -> Result<OpfsSAHPoolUtil, OpenStoreError> {
    let cfg = OpfsSAHPoolCfgBuilder::new()
        .vfs_name(&get_vfs_name(path))
        .directory(path.to_string_lossy().as_ref())
        .build();
    // Avoid global installation, due to being harder to test.
    let util = install::<sqlite_wasm_rs::WasmOsCallback>(&cfg, false).await?;

    Ok(util)
}
