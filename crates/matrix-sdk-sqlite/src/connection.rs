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
//!
//! Initially, we were using `deadpool-sqlite`, that is also using `rusqlite` as
//! the SQLite interface. However, in the implementation of
//! [`deadpool::managed::Manager`], when recycling an object (i.e. an SQLite
//! connection), [a SQL query is run to detect whether the connection is still
//! alive][connection-test]. It creates performance issues:
//!
//! 1. It runs a prepared SQL query, which has a non-negligle cost. Imagine each
//!    connection is used to run on average one query; when recycled, a second
//!    query was constantly run. Even if it's a simple query, it requires to
//!    prepare a statement, to run and to query it.
//! 2. The SQL query was run in a blocking task. Indeed,
//!    `deadpool_runtime::spawn_blocking` is used (via
//!    `deadpool_sync::SyncWrapper::interact`), which includes [blocking the
//!    thread, acquiring a lock][spawn_blocking] etc. All this has more
//!    performance cost.
//!
//! Measures have shown it is a performance bottleneck for us, especially on
//! Android. Why specifically on Android and not other systems? This is still
//! unclear at the time of writing (2025-11-11), despites having spent several
//! days digging and trying to find an answer to this question.
//!
//! We have tried to use another approach to test the aliveness of the
//! connections without running queries. It has involved patching `rusqlite` to
//! add more bindings to SQLite, and patching `deadpool` itself, but without any
//! successful results.
//!
//! Finally, we have started questioning the reason of this test: why testing
//! whether the connection was still alive? After all, there is no reason a
//! connection should die in our case:
//!
//! - all connections are local,
//! - all interactions are behind [WAL], which is local only,
//! - even if for an unknown reason the connection died, using it next time
//!   would create an error… exactly what would happen when recycling the
//!   connection.
//!
//! Consequently, we have created a new implementation of `deadpool` for
//! `rusqlite` that doesn't test the aliveness of the connections when recycled.
//! We assume they are all alive.
//!
//! This implementation is, at the time of writing (2025-11-11):
//!
//! - 3.5 times faster on Android than `deadpool-sqlite`, removing the lock and
//!   thread contention entirely,
//! - 2 times faster on iOS.
//!
//! [connection-test]: https://github.com/deadpool-rs/deadpool/blob/d6f7d58756f0cc7bdd1f3d54d820c1332d67e4d5/crates/deadpool-sqlite/src/lib.rs#L80-L100
//! [spawn_blocking]: https://github.com/deadpool-rs/deadpool/blob/d6f7d58756f0cc7bdd1f3d54d820c1332d67e4d5/crates/deadpool-sync/src/lib.rs#L113-L131
//! [WAL]: https://www.sqlite.org/wal.html

use std::{convert::Infallible, path::PathBuf};

pub use deadpool::managed::reexports::*;
use deadpool::managed::{self, Metrics, RecycleError};
use deadpool_sync::SyncWrapper;

/// The default runtime used by `matrix-sdk-sqlite` for `deadpool`.
pub const RUNTIME: Runtime = Runtime::Tokio1;

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
    database_path: PathBuf,
}

impl Manager {
    /// Creates a new [`Manager`] for a database.
    #[must_use]
    pub fn new(database_path: PathBuf) -> Self {
        Self { database_path }
    }
}

impl managed::Manager for Manager {
    type Type = SyncWrapper<rusqlite::Connection>;
    type Error = rusqlite::Error;

    async fn create(&self) -> Result<Self::Type, Self::Error> {
        let path = self.database_path.clone();
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
