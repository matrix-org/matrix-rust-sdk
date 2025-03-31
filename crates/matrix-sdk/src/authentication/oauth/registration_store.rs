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

//! OAuth 2.0 client registration store.
//!
//! This module provides a way to persist OAuth 2.0 client registrations outside
//! of the state store. This is useful when using a `Client` with an in-memory
//! store or when different store paths are used for multi-account support
//! within the same app, and those accounts need to share the same OAuth 2.0
//! client registration.

use std::{collections::HashMap, io::ErrorKind, path::PathBuf};

use oauth2::ClientId;
use ruma::serde::Raw;
use serde::{Deserialize, Serialize};
use tokio::fs;
use url::Url;

use super::ClientMetadata;

/// Errors that can occur when using the [`OAuthRegistrationStore`].
#[derive(Debug, thiserror::Error)]
pub enum OAuthRegistrationStoreError {
    /// The supplied path is not a file path.
    #[error("supplied registrations path is not a file path")]
    NotAFilePath,
    /// An error occurred when reading from or writing to the file.
    #[error(transparent)]
    File(#[from] std::io::Error),
    /// An error occurred when serializing the registration data.
    #[error("failed to serialize registration data: {0}")]
    IntoJson(serde_json::Error),
    /// An error occurred when deserializing the registration data.
    #[error("failed to deserialize registration data: {0}")]
    FromJson(serde_json::Error),
}

/// An API to store and restore OAuth 2.0 client registrations.
///
/// This stores dynamic client registrations in a file, and accepts "static"
/// client registrations via
/// [`OAuthRegistrationStore::with_static_registrations()`], for servers that
/// don't support dynamic client registration.
///
/// If the client metadata passed to this API changes, the previous
/// registrations that were stored in the file are invalidated, allowing to
/// re-register with the new metadata.
///
/// The purpose of storing client IDs outside of the state store or separate
/// from the user's session is that it allows to reuse the same client ID
/// between user sessions on the same server.
#[derive(Debug)]
pub struct OAuthRegistrationStore {
    /// The path of the file where the registrations are stored.
    pub(super) file_path: PathBuf,
    /// The metadata used to register the client.
    /// This is used to check if the client needs to be re-registered.
    pub(super) metadata: Raw<ClientMetadata>,
    /// Pre-configured registrations for use with issuers that don't support
    /// dynamic client registration.
    static_registrations: Option<HashMap<Url, ClientId>>,
}

/// The underlying data serialized into the registration file.
#[derive(Debug, Serialize, Deserialize)]
struct FrozenRegistrationData {
    /// The metadata used to register the client.
    metadata: Raw<ClientMetadata>,
    /// All of the registrations this client has made as a HashMap of issuer URL
    /// to client ID.
    dynamic_registrations: HashMap<Url, ClientId>,
}

impl OAuthRegistrationStore {
    /// Creates a new registration store.
    ///
    /// This method creates the `file`'s parent directory if it doesn't exist.
    ///
    /// # Arguments
    ///
    /// * `file` - A file path where the registrations will be stored. This
    ///   previously took a directory and stored the registrations with the path
    ///   `supplied_directory/oidc/registrations.json`.
    ///
    /// * `metadata` - The metadata used to register the client. If this changes
    ///   compared to the value stored in the file, any stored registrations
    ///   will be invalidated so the client can re-register with the new data.
    pub async fn new(
        file: PathBuf,
        metadata: Raw<ClientMetadata>,
    ) -> Result<Self, OAuthRegistrationStoreError> {
        let parent = file.parent().ok_or(OAuthRegistrationStoreError::NotAFilePath)?;
        fs::create_dir_all(parent).await?;

        Ok(OAuthRegistrationStore { file_path: file, metadata, static_registrations: None })
    }

    /// Add static registrations to the store.
    ///
    /// Static registrations are used for servers that don't support dynamic
    /// registration but provide a client ID out-of-band.
    ///
    /// These registrations are not stored in the file and must be provided each
    /// time.
    pub fn with_static_registrations(
        mut self,
        static_registrations: HashMap<Url, ClientId>,
    ) -> Self {
        self.static_registrations = Some(static_registrations);
        self
    }

    /// Returns the client ID registered for a particular issuer or `None` if a
    /// registration hasn't been made.
    ///
    /// # Arguments
    ///
    /// * `issuer` - The issuer to look up.
    ///
    /// # Errors
    ///
    /// Returns an error if the file could not be read, or if the data in the
    /// file could not be deserialized.
    pub async fn client_id(
        &self,
        issuer: &Url,
    ) -> Result<Option<ClientId>, OAuthRegistrationStoreError> {
        if let Some(client_id) =
            self.static_registrations.as_ref().and_then(|registrations| registrations.get(issuer))
        {
            return Ok(Some(client_id.clone()));
        }

        let data = self.read_registration_data().await?;
        Ok(data.and_then(|mut data| data.dynamic_registrations.remove(issuer)))
    }

    /// Stores a new client ID registration for a particular issuer.
    ///
    /// If a client ID has already been stored for the given issuer, this will
    /// overwrite the old value.
    ///
    /// # Arguments
    ///
    /// * `client_id` - The client ID obtained after registration.
    ///
    /// * `issuer` - The issuer associated with the client ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the file could not be read from or written to, or if
    /// the data in the file could not be (de)serialized.
    pub async fn set_and_write_client_id(
        &self,
        client_id: ClientId,
        issuer: Url,
    ) -> Result<(), OAuthRegistrationStoreError> {
        let mut data = self.read_registration_data().await?.unwrap_or_else(|| {
            tracing::info!("Generating new OAuth 2.0 client registration data");
            FrozenRegistrationData {
                metadata: self.metadata.clone(),
                dynamic_registrations: Default::default(),
            }
        });
        data.dynamic_registrations.insert(issuer, client_id);

        let contents = serde_json::to_vec(&data).map_err(OAuthRegistrationStoreError::IntoJson)?;
        fs::write(&self.file_path, contents).await?;

        Ok(())
    }

    /// The persisted registration data.
    ///
    /// # Errors
    ///
    /// Returns an error if the file could not be read, or if the data in the
    /// file could not be deserialized.
    async fn read_registration_data(
        &self,
    ) -> Result<Option<FrozenRegistrationData>, OAuthRegistrationStoreError> {
        let contents = match fs::read(&self.file_path).await {
            Ok(contents) => contents,
            Err(error) => {
                if error.kind() == ErrorKind::NotFound {
                    // The file doesn't exist so there is no data.
                    return Ok(None);
                }

                // Forward the error.
                return Err(error.into());
            }
        };

        let registration_data: FrozenRegistrationData =
            serde_json::from_slice(&contents).map_err(OAuthRegistrationStoreError::FromJson)?;

        if registration_data.metadata.json().get() != self.metadata.json().get() {
            tracing::info!("Metadata mismatch, ignoring any stored registrations.");
            Ok(None)
        } else {
            Ok(Some(registration_data))
        }
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::async_test;
    use tempfile::tempdir;

    use super::*;
    use crate::authentication::oauth::registration::{ApplicationType, Localized, OAuthGrantType};

    #[async_test]
    async fn test_oauth_registration_store() {
        // Given a fresh registration store with a single static registration.
        let dir = tempdir().unwrap();
        let registrations_file = dir.path().join("oauth").join("registrations.json");

        let static_url = Url::parse("https://example.com").unwrap();
        let static_id = ClientId::new("static_client_id".to_owned());
        let dynamic_url = Url::parse("https://example.org").unwrap();
        let dynamic_id = ClientId::new("dynamic_client_id".to_owned());

        let mut static_registrations = HashMap::new();
        static_registrations.insert(static_url.clone(), static_id.clone());

        let oidc_metadata = mock_metadata("Example".to_owned());

        let registrations = OAuthRegistrationStore::new(registrations_file, oidc_metadata)
            .await
            .unwrap()
            .with_static_registrations(static_registrations);

        assert_eq!(registrations.client_id(&static_url).await.unwrap(), Some(static_id.clone()));
        assert_eq!(registrations.client_id(&dynamic_url).await.unwrap(), None);

        // When a dynamic registration is added.
        registrations
            .set_and_write_client_id(dynamic_id.clone(), dynamic_url.clone())
            .await
            .unwrap();

        // Then the dynamic registration should be stored and the static registration
        // should be unaffected.
        assert_eq!(registrations.client_id(&static_url).await.unwrap(), Some(static_id));
        assert_eq!(registrations.client_id(&dynamic_url).await.unwrap(), Some(dynamic_id));
    }

    #[async_test]
    async fn test_change_of_metadata() {
        // Given a single registration with an example app name.
        let dir = tempdir().unwrap();
        let registrations_file = dir.path().join("oidc").join("registrations.json");

        let static_url = Url::parse("https://example.com").unwrap();
        let static_id = ClientId::new("static_client_id".to_owned());
        let dynamic_url = Url::parse("https://example.org").unwrap();
        let dynamic_id = ClientId::new("dynamic_client_id".to_owned());

        let oidc_metadata = mock_metadata("Example".to_owned());

        let mut static_registrations = HashMap::new();
        static_registrations.insert(static_url.clone(), static_id.clone());

        let registrations = OAuthRegistrationStore::new(registrations_file.clone(), oidc_metadata)
            .await
            .unwrap()
            .with_static_registrations(static_registrations.clone());
        registrations
            .set_and_write_client_id(dynamic_id.clone(), dynamic_url.clone())
            .await
            .unwrap();

        assert_eq!(registrations.client_id(&static_url).await.unwrap(), Some(static_id.clone()));
        assert_eq!(registrations.client_id(&dynamic_url).await.unwrap(), Some(dynamic_id));

        // When the app name changes.
        let new_oidc_metadata = mock_metadata("New App".to_owned());

        let registrations = OAuthRegistrationStore::new(registrations_file, new_oidc_metadata)
            .await
            .unwrap()
            .with_static_registrations(static_registrations);

        // Then the dynamic registrations are cleared.
        assert_eq!(registrations.client_id(&dynamic_url).await.unwrap(), None);
        assert_eq!(registrations.client_id(&static_url).await.unwrap(), Some(static_id));
    }

    fn mock_metadata(client_name: String) -> Raw<ClientMetadata> {
        let callback_url = Url::parse("https://example.org/login/callback").unwrap();
        let client_uri = Url::parse("https://example.org/").unwrap();

        let mut metadata = ClientMetadata::new(
            ApplicationType::Web,
            vec![OAuthGrantType::AuthorizationCode { redirect_uris: vec![callback_url] }],
            Localized::new(client_uri, None),
        );
        metadata.client_name = Some(Localized::new(client_name, None));

        Raw::new(&metadata).unwrap()
    }
}
