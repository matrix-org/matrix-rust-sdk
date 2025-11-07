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

//! An implementation of `deadpool` for `rusqlite`.

use std::{convert::Infallible, path::PathBuf};

pub use deadpool::managed::reexports::*;
use deadpool::managed::{self, Metrics, PoolConfig, RecycleError};
use deadpool_sync::SyncWrapper;

/// The default runtime used by `matrix-sdk-sqlite` for `deadpool`.
const RUNTIME: Runtime = Runtime::Tokio1;

/// The configuration of a connection.
#[derive(Clone, Debug)]
pub struct Config {
    path: PathBuf,
    pool_config: PoolConfig,
}

impl Config {
    /// Create a new [`Config`].
    ///
    /// `path` represents the path to the database. `pool_config` represents the
    /// [`PoolConfig`].
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, pool_config: PoolConfig) -> Self {
        Self { path: path.into(), pool_config }
    }

    /// Creates a new [`Pool`].
    ///
    /// # Errors
    ///
    /// See [`CreatePoolError`] for details.
    pub fn create_pool(&self) -> Result<Pool, managed::CreatePoolError<Infallible>> {
        let manager = Manager::from_config(self);

        Pool::builder(manager)
            .config(self.pool_config)
            .runtime(RUNTIME)
            .build()
            .map_err(CreatePoolError::Build)
    }
}

deadpool::managed_reexports!(
    "matrix-sdk-sqlite",
    Manager,
    managed::Object<Manager>,
    rusqlite::Error,
    Infallible
);

/// Type representing a connection to SQLite from the [`Pool`].
pub type Connection = Object;

/// [`Manager`][managed::Manager] for creating and recycling SQLite
/// [`Connection`]s.
#[derive(Debug)]
pub struct Manager {
    config: Config,
}

impl Manager {
    /// Creates a new [`Manager`] using the given [`Config`] backed by the
    /// specified [`Runtime`].
    #[must_use]
    fn from_config(config: &Config) -> Self {
        Self { config: config.clone() }
    }
}

impl managed::Manager for Manager {
    type Type = SyncWrapper<rusqlite::Connection>;
    type Error = rusqlite::Error;

    async fn create(&self) -> Result<Self::Type, Self::Error> {
        let path = self.config.path.clone();
        SyncWrapper::new(RUNTIME, move || rusqlite::Connection::open(path)).await
    }

    async fn recycle(
        &self,
        conn: &mut Self::Type,
        _: &Metrics,
    ) -> managed::RecycleResult<Self::Error> {
        if conn.is_mutex_poisoned() {
            return Err(RecycleError::Message(
                "Mutex is poisoned. Connection is considered unusable.".into(),
            ));
        }

        Ok(())
    }
}
