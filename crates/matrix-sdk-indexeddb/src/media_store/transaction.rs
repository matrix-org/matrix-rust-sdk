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

use std::ops::Deref;

use indexed_db_futures::prelude::IdbTransaction;
use matrix_sdk_base::media::store::MediaRetentionPolicy;

use crate::{
    media_store::{
        serializer::indexed_types::{IndexedCoreIdKey, IndexedLeaseIdKey},
        types::Lease,
    },
    serializer::IndexedTypeSerializer,
    transaction::{Transaction, TransactionError},
};

/// Represents an IndexedDB transaction, but provides a convenient interface for
/// performing operations relevant to the IndexedDB implementation of
/// [`MediaStore`](matrix_sdk_base::media::store::MediaStore).
pub struct IndexeddbMediaStoreTransaction<'a> {
    transaction: Transaction<'a>,
}

impl<'a> Deref for IndexeddbMediaStoreTransaction<'a> {
    type Target = Transaction<'a>;

    fn deref(&self) -> &Self::Target {
        &self.transaction
    }
}

impl<'a> IndexeddbMediaStoreTransaction<'a> {
    pub fn new(transaction: IdbTransaction<'a>, serializer: &'a IndexedTypeSerializer) -> Self {
        Self { transaction: Transaction::new(transaction, serializer) }
    }

    /// Returns the underlying IndexedDB transaction.
    pub fn into_inner(self) -> Transaction<'a> {
        self.transaction
    }

    /// Commit all operations tracked in this transaction to IndexedDB.
    pub async fn commit(self) -> Result<(), TransactionError> {
        self.transaction.commit().await
    }

    /// Query IndexedDB for the lease that matches the given key `id`. If more
    /// than one lease is found, an error is returned.
    pub async fn get_lease_by_id(&self, id: &str) -> Result<Option<Lease>, TransactionError> {
        self.transaction.get_item_by_key_components::<Lease, IndexedLeaseIdKey>(id).await
    }

    /// Puts a lease into IndexedDB. If a media with the same key already
    /// exists, it will be overwritten.
    pub async fn put_lease(&self, lease: &Lease) -> Result<(), TransactionError> {
        self.transaction.put_item(lease).await
    }

    /// Query IndexedDB for the stored [`MediaRetentionPolicy`]
    pub async fn get_media_retention_policy(
        &self,
    ) -> Result<Option<MediaRetentionPolicy>, TransactionError> {
        self.transaction
            .get_item_by_key_components::<MediaRetentionPolicy, IndexedCoreIdKey>(())
            .await
    }
}
