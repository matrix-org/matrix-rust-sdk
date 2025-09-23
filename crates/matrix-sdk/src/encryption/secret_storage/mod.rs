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

//! Secret Storage Support
//!
//! This submodule provides essential functionality for secret storage in
//! compliance with the [Matrix protocol specification][spec].
//!
//! Secret storage is a critical component that provides an encrypted
//! key/value storage system. It leverages [account data] events stored on the
//! Matrix homeserver to ensure secure and private storage of sensitive
//! information.
//!
//! For detailed information and usage guidelines, refer to the documentation of
//! the [`SecretStore`] struct.
//!
//! # Examples
//!
//! ```no_run
//! # use matrix_sdk::Client;
//! # use url::Url;
//! # async {
//! # let homeserver = Url::parse("http://example.com")?;
//! # let client = Client::new(homeserver).await?;
//! use ruma::events::secret::request::SecretName;
//!
//! // Open the store.
//! let secret_store = client
//!     .encryption()
//!     .secret_storage()
//!     .open_secret_store("It's a secret to everybody")
//!     .await?;
//!
//! // Import the secrets.
//! secret_store.import_secrets().await?;
//!
//! // Our own device should now be verified.
//! let device = client
//!     .encryption()
//!     .get_own_device()
//!     .await?
//!     .expect("We should be able to retrieve our own device");
//!
//! assert!(device.is_cross_signed_by_owner());
//!
//! # anyhow::Ok(()) };
//! ```
//!
//! [spec]: https://spec.matrix.org/v1.8/client-server-api/#secret-storage
//! [account data]: https://spec.matrix.org/v1.8/client-server-api/#client-config

use std::string::FromUtf8Error;

use matrix_sdk_base::crypto::{
    CryptoStoreError, SecretImportError,
    secret_storage::{DecodeError, MacError, SecretStorageKey},
};
use ruma::{
    events::{
        EventContentFromType, GlobalAccountDataEventType,
        secret::request::SecretName,
        secret_storage::{
            default_key::SecretStorageDefaultKeyEventContent, key::SecretStorageKeyEventContent,
        },
    },
    serde::Raw,
};
use serde_json::value::to_raw_value;
use thiserror::Error;

use super::identities::ManualVerifyError;
use crate::Client;

mod futures;
mod secret_store;

pub use futures::CreateStore;
pub use secret_store::SecretStore;

/// Convenicence type alias for the secret-storage specific results.
pub type Result<T, E = SecretStorageError> = std::result::Result<T, E>;

/// Error type for errors when importing a secret from secret storage.
#[derive(Debug, Error)]
pub enum ImportError {
    /// A typical SDK error.
    #[error(transparent)]
    Sdk(#[from] crate::Error),

    /// Error when deserializing account data events.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// The key that we tried to import was invalid.
    #[error(transparent)]
    Key(vodozemac::KeyError),

    /// The public key of the imported private key doesn't match the public
    /// key that was uploaded to the server.
    #[error(
        "The public key of the imported private key doesn't match the public\
            key that was uploaded to the server"
    )]
    MismatchedPublicKeys,

    /// Error describing a decryption failure of a secret.
    #[error(transparent)]
    Decryption(#[from] DecryptionError),
}

/// Error type for the secret-storage subsystem.
#[derive(Debug, Error)]
pub enum SecretStorageError {
    /// A typical SDK error.
    #[error(transparent)]
    Sdk(#[from] crate::Error),

    /// Error when deserializing account data events.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// The secret storage key could not have been decoded or verified
    /// successfully.
    #[error(transparent)]
    SecretStorageKey(#[from] DecodeError),

    /// The secret store could not be opened because info about the
    /// secret-storage key could not have been found in the account data of
    /// the user.
    #[error(
        "The info about the secret key could not have been found in the account data of the user"
    )]
    MissingKeyInfo {
        /// The key ID of the default key. Will be set to the key ID in the
        /// `m.secret_storage.default_key` event. If the
        /// `m.secret_storage.default_key` does not exits, will be
        /// `None`.
        key_id: Option<String>,
    },

    /// An error when importing from the secret store into the local store.
    #[error("Error while importing {name}: {error}")]
    ImportError {
        /// The name of the secret that was being imported when the error
        /// occurred.
        name: SecretName,
        /// The error that occurred.
        error: ImportError,
    },

    /// A general storage error.
    #[error(transparent)]
    Storage(#[from] CryptoStoreError),

    /// An error happened while trying to mark our own device as verified after
    /// the private cross-signing keys have been imported.
    #[error(transparent)]
    Verification(#[from] ManualVerifyError),

    /// Error describing a decryption failure of a secret.
    #[error(transparent)]
    Decryption(#[from] DecryptionError),
}

impl SecretStorageError {
    /// Create a `SecretStorageError::ImportError` from a secret name and any
    /// error that can be converted directly into an `ImportError`
    fn into_import_error(secret_name: SecretName, error: impl Into<ImportError>) -> Self {
        SecretStorageError::ImportError { name: secret_name, error: error.into() }
    }

    /// Create a `SecretStorageError` from a `SecretImportError`
    ///
    /// `SecretImportError::Key` and `SecretImportError::MismatchedPublicKeys`
    /// become `SecretStorageError::ImportError`s, whereas
    /// `SecretImportError::Store` becomes `SecretStorageError::Storage` since
    /// the error is with the crypto storage rather than in importing the
    /// secret.
    fn from_secret_import_error(error: SecretImportError) -> Self {
        match error {
            SecretImportError::Key { name, error } => {
                SecretStorageError::ImportError { name, error: ImportError::Key(error) }
            }
            SecretImportError::MismatchedPublicKeys { name } => {
                SecretStorageError::ImportError { name, error: ImportError::MismatchedPublicKeys }
            }
            SecretImportError::Store(error) => SecretStorageError::Storage(error),
        }
    }
}

/// Error type describing decryption failures of the secret-storage system.
#[derive(Debug, Error)]
pub enum DecryptionError {
    /// The secret could not have been decrypted.
    #[error("Could not decrypt the secret using the secret storage key, invalid MAC.")]
    Mac(#[from] MacError),

    /// Could not decode the secret, the secret is not valid UTF-8.
    #[error("Could not decode the secret, the secret is not valid UTF-8")]
    Utf8(#[from] FromUtf8Error),
}

/// A high-level API to manage secret storage.
///
/// To get this, use
/// [`client.encryption().secret_storage()`](super::Encryption::secret_storage).
#[derive(Debug)]
pub struct SecretStorage {
    pub(super) client: Client,
}

impl SecretStorage {
    /// Open the [`SecretStore`] with the given `key`.
    ///
    /// The `secret_storage_key` can be a passphrase or a Base58 encoded secret
    /// storage key.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// use ruma::events::secret::request::SecretName;
    ///
    /// let secret_store = client
    ///     .encryption()
    ///     .secret_storage()
    ///     .open_secret_store("It's a secret to everybody")
    ///     .await?;
    ///
    /// let my_secret = "Top secret secret";
    /// let my_secret_name = "m.treasure";
    ///
    /// secret_store.put_secret(my_secret_name, my_secret);
    ///
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn open_secret_store(&self, secret_storage_key: &str) -> Result<SecretStore> {
        let maybe_default_key_id = self.fetch_default_key_id().await?;

        if let Some(default_key_id) = maybe_default_key_id {
            let default_key_id = default_key_id.deserialize()?;

            let event_type =
                GlobalAccountDataEventType::SecretStorageKey(default_key_id.key_id.to_owned());
            let secret_key =
                self.client.account().fetch_account_data(event_type.to_owned()).await?;

            if let Some(secret_key_content) = secret_key {
                let event_type = event_type.to_string();
                let secret_key_content = to_raw_value(&secret_key_content)?;

                let secret_key_content =
                    SecretStorageKeyEventContent::from_parts(&event_type, &secret_key_content)?;

                let key =
                    SecretStorageKey::from_account_data(secret_storage_key, secret_key_content)?;

                Ok(SecretStore { client: self.client.to_owned(), key })
            } else {
                Err(SecretStorageError::MissingKeyInfo { key_id: Some(default_key_id.key_id) })
            }
        } else {
            Err(SecretStorageError::MissingKeyInfo { key_id: None })
        }
    }

    /// Create a new [`SecretStore`].
    ///
    /// The [`SecretStore`] will be protected by a randomly generated key, or
    /// optionally a passphrase can be provided as well.
    ///
    /// In both cases, whether a passphrase was provided or not, the key to open
    /// the [`SecretStore`] can be obtained using the
    /// [`SecretStore::secret_storage_key()`] method.
    ///
    /// *Note*: This method will set the new secret storage key as the default
    /// key in the `m.secret_storage.default_key` event. All the known secrets
    /// will be re-encrypted and uploaded to the homeserver as well. This
    /// includes the following secrets:
    ///
    /// - `m.cross_signing.master`: The master cross-signing key.
    /// - `m.cross_signing.self_signing`: The self-signing cross-signing key.
    /// - `m.cross_signing.user_signing`: The user-signing cross-signing key.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// use ruma::events::secret::request::SecretName;
    ///
    /// let secret_store = client
    ///     .encryption()
    ///     .secret_storage()
    ///     .create_secret_store()
    ///     .await?;
    ///
    /// let my_secret = "Top secret secret";
    /// let my_secret_name = SecretName::from("m.treasure");
    ///
    /// secret_store.put_secret(my_secret_name, my_secret);
    ///
    /// let secret_storage_key = secret_store.secret_storage_key();
    ///
    /// println!("Your secret storage key is {secret_storage_key}, save it somewhere safe.");
    ///
    /// # anyhow::Ok(()) };
    /// ```
    pub fn create_secret_store(&self) -> CreateStore<'_> {
        CreateStore { secret_storage: self, passphrase: None }
    }

    /// Run a network request to find if secret storage is set up for this user.
    pub async fn is_enabled(&self) -> crate::Result<bool> {
        if let Some(content) = self.fetch_default_key_id().await? {
            // Since we can't delete account data events, we're going to treat
            // deserialization failures as secret storage being disabled.
            Ok(content.deserialize().is_ok())
        } else {
            // No account data event found, must be disabled.
            Ok(false)
        }
    }

    /// Fetch the `m.secret_storage.default_key` event from the server.
    pub async fn fetch_default_key_id(
        &self,
    ) -> crate::Result<Option<Raw<SecretStorageDefaultKeyEventContent>>> {
        self.client
            .account()
            .fetch_account_data_static::<SecretStorageDefaultKeyEventContent>()
            .await
    }
}
