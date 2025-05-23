// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use std::{future::IntoFuture, pin::Pin};

use futures_core::Future;
use matrix_sdk_base::crypto::secret_storage::SecretStorageKey;
use ruma::events::secret_storage::default_key::SecretStorageDefaultKeyEventContent;

use super::{Result, SecretStorage, SecretStore};

/// Future returned by [`SecretStorage::create_secret_store()`].
#[derive(Debug)]
pub struct CreateStore<'a> {
    pub(super) secret_storage: &'a SecretStorage,
    pub(super) passphrase: Option<&'a str>,
}

impl<'a> CreateStore<'a> {
    /// Set the passphrase for the new [`SecretStore`].
    ///
    /// See the documentation for the [`SecretStorage::create_secret_store()`]
    /// method for more info.
    pub fn with_passphrase(mut self, passphrase: &'a str) -> Self {
        self.passphrase = Some(passphrase);

        self
    }
}

impl<'a> IntoFuture for CreateStore<'a> {
    type Output = Result<SecretStore>;
    #[cfg(target_family = "wasm")]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + 'a>>;
    #[cfg(not(target_family = "wasm"))]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        let Self { secret_storage, passphrase } = self;

        Box::pin(async move {
            // Prevent multiple simultaneous calls to this method.
            //
            // See the documentation for the lock in the `store_secret` method for more
            // info.
            let client_copy = secret_storage.client.to_owned();
            let _guard = client_copy.locks().open_secret_store_lock.lock().await;

            let new_key = if let Some(passphrase) = passphrase {
                SecretStorageKey::new_from_passphrase(passphrase)
            } else {
                SecretStorageKey::new()
            };

            let content = new_key.event_content().to_owned();

            secret_storage.client.account().set_account_data(content).await?;

            let store = SecretStore { client: secret_storage.client.to_owned(), key: new_key };
            store.export_secrets().await?;

            let default_key_content =
                SecretStorageDefaultKeyEventContent::new(store.key.key_id().to_owned());

            store.client.account().set_account_data(default_key_content).await?;

            Ok(store)
        })
    }
}
