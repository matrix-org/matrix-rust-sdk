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

//! OpenID Connect client registration management.
//!
//! This module provides a way to persist OIDC client registrations outside of
//! the state store. This is useful when using a `Client` with an in-memory
//! store or when different store paths are used for multi-account support
//! within the same app, and those accounts need to share the same OIDC client
//! registration.

use std::{
    collections::HashMap,
    fs,
    fs::File,
    io::{BufReader, BufWriter},
    path::{Path, PathBuf},
};

use mas_oidc_client::types::registration::{
    ClientMetadata, ClientMetadataVerificationError, VerifiedClientMetadata,
};
use serde::{Deserialize, Serialize};
use url::Url;

/// Errors related to persisting OIDC registrations.
#[derive(Debug, thiserror::Error)]
pub enum OidcRegistrationsError {
    /// The supplied registrations file path is invalid.
    #[error("Failed to use the supplied registrations file path.")]
    InvalidFilePath,
    /// An error occurred whilst saving the registration data.
    #[error("Failed to save the registration data {0}.")]
    SaveFailure(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// A client ID that has been registered with an OpenID Connect provider.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct ClientId(pub String);

/// The data needed to restore an OpenID Connect session.
#[derive(Debug)]
pub struct OidcRegistrations {
    /// The path of the file where the registrations are stored.
    file_path: PathBuf,
    /// The hash for the metadata used to register the client.
    /// This is used to check if the client needs to be re-registered.
    verified_metadata: VerifiedClientMetadata,
    /// Pre-configured registrations for use with issuers that don't support
    /// dynamic client registration.
    static_registrations: HashMap<Url, ClientId>,
}

/// The underlying data serialized into the registration file.
#[derive(Debug, Serialize)]
struct FrozenRegistrationData {
    /// The hash for the metadata used to register the client.
    metadata: VerifiedClientMetadata,
    /// All of the registrations this client has made as a HashMap of issuer URL
    /// (as a string) to client ID (as a string).
    dynamic_registrations: HashMap<Url, ClientId>,
}

/// The deserialize data from the registration file. This data needs to be
/// validated before it can be used.
#[derive(Debug, Deserialize)]
struct UnvalidatedRegistrationData {
    /// The hash for the metadata used to register the client.
    metadata: ClientMetadata,
    /// All of the registrations this client has made as a HashMap of issuer URL
    /// (as a string) to client ID (as a string).
    dynamic_registrations: HashMap<Url, ClientId>,
}

impl UnvalidatedRegistrationData {
    /// Validates the registration data, returning a `FrozenRegistrationData`.
    fn validate(&self) -> Result<FrozenRegistrationData, ClientMetadataVerificationError> {
        let verified_metadata = match self.metadata.clone().validate() {
            Ok(metadata) => metadata,
            Err(e) => {
                tracing::warn!("Failed to validate stored metadata.");
                return Err(e);
            }
        };

        Ok(FrozenRegistrationData {
            metadata: verified_metadata,
            dynamic_registrations: self.dynamic_registrations.clone(),
        })
    }
}

/// Manages the storage of OIDC registrations.
impl OidcRegistrations {
    /// Creates a new registration store.
    ///
    /// # Arguments
    ///
    /// * `registrations_file` - A file path where the registrations will be
    ///   stored. This previously took a directory and stored the registrations
    ///   with the path `supplied_directory/oidc/registrations.json`.
    ///
    /// * `metadata` - The metadata used to register the client. If this
    ///   changes, any stored registrations will be lost so the client can
    ///   re-register with the new data.
    ///
    /// * `static_registrations` - Pre-configured registrations for use with
    ///   issuers that don't support dynamic client registration.
    pub fn new(
        registrations_file: &Path,
        metadata: VerifiedClientMetadata,
        static_registrations: HashMap<Url, ClientId>,
    ) -> Result<Self, OidcRegistrationsError> {
        let parent = registrations_file.parent().ok_or(OidcRegistrationsError::InvalidFilePath)?;
        fs::create_dir_all(parent).map_err(|_| OidcRegistrationsError::InvalidFilePath)?;

        Ok(OidcRegistrations {
            file_path: registrations_file.to_owned(),
            verified_metadata: metadata,
            static_registrations,
        })
    }

    /// Returns the client ID registered for a particular issuer or None if a
    /// registration hasn't been made.
    pub fn client_id(&self, issuer: &Url) -> Option<ClientId> {
        let mut data = self.read_or_generate_registration_data();
        data.dynamic_registrations.extend(self.static_registrations.clone());
        data.dynamic_registrations.get(issuer).cloned()
    }

    /// Stores a new client ID registration for a particular issuer. If a client
    /// ID has already been stored, this will overwrite the old value.
    pub fn set_and_write_client_id(
        &self,
        client_id: ClientId,
        issuer: Url,
    ) -> Result<(), OidcRegistrationsError> {
        let mut data = self.read_or_generate_registration_data();
        data.dynamic_registrations.insert(issuer, client_id);

        let writer = BufWriter::new(
            File::create(&self.file_path)
                .map_err(|e| OidcRegistrationsError::SaveFailure(Box::new(e)))?,
        );
        serde_json::to_writer(writer, &data)
            .map_err(|e| OidcRegistrationsError::SaveFailure(Box::new(e)))
    }

    /// Returns the underlying registration data, or generates a new one.
    fn read_or_generate_registration_data(&self) -> FrozenRegistrationData {
        let try_read_previous = || {
            let reader = BufReader::new(
                File::open(&self.file_path)
                    .map_err(|error| {
                        tracing::warn!("Failed to load registrations file: {error}");
                    })
                    .ok()?,
            );

            let registration_data: UnvalidatedRegistrationData = serde_json::from_reader(reader)
                .map_err(|error| {
                    tracing::warn!("Failed to deserialize registrations file: {error}");
                })
                .ok()?;

            let registration_data = registration_data
                .validate()
                .map_err(|error| {
                    tracing::warn!("Failed to validate registration data: {error}");
                })
                .ok()?;

            if registration_data.metadata != self.verified_metadata {
                tracing::warn!("Metadata mismatch, ignoring any stored registrations.");
                return None;
            }

            Some(registration_data)
        };

        try_read_previous().unwrap_or_else(|| {
            tracing::warn!("Generating new registration data");
            FrozenRegistrationData {
                metadata: self.verified_metadata.clone(),
                dynamic_registrations: Default::default(),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use mas_oidc_client::types::registration::Localized;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn test_oidc_registrations() {
        // Given a fresh registration store with a single static registration.
        let dir = tempdir().unwrap();
        let registrations_file = dir.path().join("oidc").join("registrations.json");

        let static_url = Url::parse("https://example.com").unwrap();
        let static_id = ClientId("static_client_id".to_owned());
        let dynamic_url = Url::parse("https://example.org").unwrap();
        let dynamic_id = ClientId("dynamic_client_id".to_owned());

        let mut static_registrations = HashMap::new();
        static_registrations.insert(static_url.clone(), static_id.clone());

        let oidc_metadata = mock_metadata("Example".to_owned());

        let registrations =
            OidcRegistrations::new(&registrations_file, oidc_metadata, static_registrations)
                .unwrap();

        assert_eq!(registrations.client_id(&static_url), Some(static_id.clone()));
        assert_eq!(registrations.client_id(&dynamic_url), None);

        // When a dynamic registration is added.
        registrations.set_and_write_client_id(dynamic_id.clone(), dynamic_url.clone()).unwrap();

        // Then the dynamic registration should be stored and the static registration
        // should be unaffected.
        assert_eq!(registrations.client_id(&static_url), Some(static_id));
        assert_eq!(registrations.client_id(&dynamic_url), Some(dynamic_id));
    }

    #[test]
    fn test_change_of_metadata() {
        // Given a single registration with an example app name.
        let dir = tempdir().unwrap();
        let registrations_file = dir.path().join("oidc").join("registrations.json");

        let static_url = Url::parse("https://example.com").unwrap();
        let static_id = ClientId("static_client_id".to_owned());
        let dynamic_url = Url::parse("https://example.org").unwrap();
        let dynamic_id = ClientId("dynamic_client_id".to_owned());

        let oidc_metadata = mock_metadata("Example".to_owned());

        let mut static_registrations = HashMap::new();
        static_registrations.insert(static_url.clone(), static_id.clone());

        let registrations = OidcRegistrations::new(
            &registrations_file,
            oidc_metadata,
            static_registrations.clone(),
        )
        .unwrap();
        registrations.set_and_write_client_id(dynamic_id.clone(), dynamic_url.clone()).unwrap();

        assert_eq!(registrations.client_id(&static_url), Some(static_id.clone()));
        assert_eq!(registrations.client_id(&dynamic_url), Some(dynamic_id));

        // When the app name changes.
        let new_oidc_metadata = mock_metadata("New App".to_owned());

        let registrations =
            OidcRegistrations::new(&registrations_file, new_oidc_metadata, static_registrations)
                .unwrap();

        // Then the dynamic registrations are cleared.
        assert_eq!(registrations.client_id(&dynamic_url), None);
        assert_eq!(registrations.client_id(&static_url), Some(static_id));
    }

    fn mock_metadata(client_name: String) -> VerifiedClientMetadata {
        let callback_url = Url::parse("https://example.org/login/callback").unwrap();
        let client_name = Some(Localized::new(client_name, None));

        ClientMetadata {
            redirect_uris: Some(vec![callback_url]),
            client_name,
            ..Default::default()
        }
        .validate()
        .unwrap()
    }
}
