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
// limitations under the License

//! A reopenable handle to an IndexedDB database, shared by every IndexedDB store
//! in this crate.
//!
//! The browser can close an `IDBDatabase` out from under us — a transient
//! storage backend error, eviction under storage pressure, or a `versionchange`
//! from another browsing context. Once that happens, the handle is permanently
//! unusable: every subsequent transaction fails for the lifetime of the
//! process, leaving the store unable to read or write until the app restarts.
//!
//! [`IndexeddbConnection`] holds the handle behind shared interior mutability so
//! it can be reopened in place, with the fresh handle observed by every clone of
//! the owning store, and provides a [`with_transaction`] helper that reopens and
//! retries once when it detects that the connection was closed.
//!
//! [`with_transaction`]: IndexeddbConnection::with_transaction

use std::{cell::RefCell, future::Future, rc::Rc};

use indexed_db_futures::{
    Build,
    database::Database,
    error::{DomException, Error},
    transaction::{Transaction, TransactionMode},
};

/// A reopenable handle to a single IndexedDB database.
#[derive(Debug, Clone)]
pub struct IndexeddbConnection {
    /// The name of the database, retained so the connection can be reopened.
    name: String,
    /// The live handle. The outer `Rc` is shared by every clone of the owning
    /// store; the inner `Rc` is the handle itself, cloned out for the duration
    /// of each transaction so a concurrent reopen can swap the slot without
    /// invalidating an in-flight transaction.
    inner: Rc<RefCell<Rc<Database>>>,
}

impl IndexeddbConnection {
    /// Wrap a freshly opened database in a reopenable connection.
    pub fn new(name: String, database: Database) -> Self {
        Self { name, inner: Rc::new(RefCell::new(Rc::new(database))) }
    }

    /// The name of the underlying database.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The current live handle.
    pub fn database(&self) -> Rc<Database> {
        self.inner.borrow().clone()
    }

    /// Reopen the connection via `open`, replacing the shared handle.
    ///
    /// Exposed for the `reopen()` store-trait methods; the transaction path
    /// reopens lazily via [`with_transaction`](Self::with_transaction).
    pub async fn reopen<E, Open, OpenFut>(&self, open: Open) -> Result<(), E>
    where
        Open: FnOnce(String) -> OpenFut,
        OpenFut: Future<Output = Result<Database, E>>,
    {
        let database = Rc::new(open(self.name.clone()).await?);
        *self.inner.borrow_mut() = database;
        Ok(())
    }

    /// Run `body` against a fresh transaction over `stores`.
    ///
    /// If building the transaction fails because the connection has been closed
    /// by the browser, the handle is reopened via `open` and the transaction is
    /// retried exactly once. Any other failure — and any error returned by
    /// `body` itself — is propagated unchanged.
    ///
    /// The owning `Rc<Database>` is kept alive on this stack frame for the whole
    /// duration of `body`, so the transaction handed to `body` borrows a handle
    /// that outlives it. This keeps a transient connection loss from
    /// permanently bricking the store without any `unsafe` or background task.
    pub async fn with_transaction<R, E, Open, OpenFut, Body>(
        &self,
        stores: &[&str],
        mode: TransactionMode,
        open: Open,
        body: Body,
    ) -> Result<R, E>
    where
        E: From<Error>,
        Open: FnOnce(String) -> OpenFut,
        OpenFut: Future<Output = Result<Database, E>>,
        Body: AsyncFnOnce(Transaction<'_>) -> Result<R, E>,
    {
        // First attempt on the current handle. The `Ref` from `borrow()` is
        // released immediately; only the cloned `Rc` is held across the await.
        {
            let database = self.inner.borrow().clone();
            match database.transaction(stores).with_mode(mode).build() {
                Ok(transaction) => return body(transaction).await,
                Err(error) => {
                    if !is_connection_closed(&error) {
                        return Err(error.into());
                    }
                    // The connection was closed; fall through to reopen and
                    // retry once. `database` is dropped at the end of this block.
                }
            }
        }

        let database = Rc::new(open(self.name.clone()).await?);
        *self.inner.borrow_mut() = database.clone();

        let transaction = database.transaction(stores).with_mode(mode).build()?;
        body(transaction).await
    }
}

/// Whether `error` indicates that the underlying IndexedDB connection is no
/// longer usable (closing or closed), as opposed to a logical error on an
/// otherwise healthy connection.
///
/// Building a transaction on a closed `IDBDatabase` raises a DOM
/// `InvalidStateError`; a connection that dies as the transaction is being set
/// up can also surface as `TransactionInactiveError` or `AbortError`. Any of
/// these means the handle must be reopened before retrying.
pub fn is_connection_closed(error: &Error) -> bool {
    matches!(
        error,
        Error::DomException(
            DomException::InvalidStateError(_)
                | DomException::TransactionInactiveError(_)
                | DomException::AbortError(_)
        )
    )
}
