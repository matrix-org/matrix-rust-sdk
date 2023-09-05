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

use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    fmt, fs,
    fs::File,
    hash::{Hash, Hasher},
    io::{BufReader, BufWriter},
    path::PathBuf,
};

use matrix_sdk::oidc::types::registration::VerifiedClientMetadata;
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum OidcRegistrationsError {
    #[error("Failed to use the supplied base path.")]
    InvalidBasePath,
}

/// A client ID that has been registered with an OpenID Connect provider.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct ClientId(pub String);

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The data needed to restore an OpenID Connect session.
#[derive(Debug)]
pub struct OidcRegistrations {
    /// The path of the file where the registrations are stored.
    file_path: PathBuf,
    /// The hash for the metadata used to register the client.
    /// This is used to check if the client needs to be re-registered.
    metadata_hash: u64,
    /// Pre-configured registrations for use with issuers that don't support
    /// dynamic client registration.
    static_registrations: HashMap<Url, ClientId>,
}

/// The underlying data that is stored in the registration file.
#[derive(Debug, Serialize, Deserialize)]
struct FrozenRegistrationData {
    /// The hash for the metadata used to register the client.
    metadata_hash: u64,
    /// All of the registrations this client has made as a HashMap of issuer URL
    /// (as a string) to client ID (as a string).
    dynamic_registrations: HashMap<Url, ClientId>,
}

/// Manages the storage of OIDC registrations.
impl OidcRegistrations {
    /// Creates a new registration store.
    ///
    /// # Arguments
    ///
    /// * `base_path` - A directory where the registrations file can be stored.
    ///   It will be nested inside of a directory called `oidc` as
    ///   `registrations.json`.
    ///
    /// * `metadata` - The metadata used to register the client. If this
    ///   changes, any stored registrations will be lost so the client can
    ///   re-register with the new data.
    ///
    /// * `static_registrations` - Pre-configured registrations for use with
    ///   issuers that don't support dynamic client registration.
    pub fn new(
        base_path: &str,
        metadata: &VerifiedClientMetadata,
        static_registrations: HashMap<Url, ClientId>,
    ) -> Result<Self, OidcRegistrationsError> {
        let oidc_directory = PathBuf::from(base_path).join("oidc");
        fs::create_dir_all(&oidc_directory).map_err(|_| OidcRegistrationsError::InvalidBasePath)?;

        let metadata_hash = {
            let mut hasher = DefaultHasher::new();
            let metadata_string = serde_json::to_string(metadata).unwrap();
            metadata_string.hash(&mut hasher);
            hasher.finish()
        };

        Ok(OidcRegistrations {
            file_path: oidc_directory.join("registrations.json"),
            metadata_hash,
            static_registrations,
        })
    }

    /// Returns the underlying registration data.
    fn read_registration_data(&self) -> FrozenRegistrationData {
        let reader = match File::open(&self.file_path) {
            Ok(file) => BufReader::new(file),
            Err(e) => {
                tracing::warn!("Failed to open registrations file: {e}");
                return FrozenRegistrationData {
                    metadata_hash: self.metadata_hash,
                    dynamic_registrations: Default::default(),
                };
            }
        };

        let registration_data = match serde_json::from_reader::<_, FrozenRegistrationData>(reader) {
            Ok(data) => data,
            Err(e) => {
                tracing::error!("Failed to parse registrations file: {e}");
                return FrozenRegistrationData {
                    metadata_hash: self.metadata_hash,
                    dynamic_registrations: Default::default(),
                };
            }
        };

        if registration_data.metadata_hash != self.metadata_hash {
            tracing::warn!("Metadata hash mismatch, clearing registrations");
            return FrozenRegistrationData {
                metadata_hash: self.metadata_hash,
                dynamic_registrations: Default::default(),
            };
        }

        registration_data
    }

    /// Returns the client ID registered for a particular issuer or None if a
    /// registration hasn't been made.
    pub fn client_id(&self, issuer: &Url) -> Option<ClientId> {
        let mut data = self.read_registration_data();
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
        let mut data = self.read_registration_data();
        data.dynamic_registrations.insert(issuer, client_id);

        let writer = BufWriter::new(
            File::create(&self.file_path).map_err(|_| OidcRegistrationsError::InvalidBasePath)?,
        );
        serde_json::to_writer(writer, &data).map_err(|_| OidcRegistrationsError::InvalidBasePath)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, default::Default};

    use matrix_sdk::oidc::types::registration::{ClientMetadata, Localized};
    use tempfile::tempdir;
    use wiremock::http::Url;

    use super::*;

    #[test]
    fn test_oidc_registrations() {
        // Given a fresh registration store with a single static registration.
        let dir = tempdir().unwrap();
        let base_path = dir.path().to_str().unwrap();

        let static_url = Url::parse("https://example.com").unwrap();
        let static_id = ClientId("static_client_id".to_owned());
        let dynamic_url = Url::parse("https://example.org").unwrap();
        let dynamic_id = ClientId("dynamic_client_id".to_owned());

        let mut static_registrations = HashMap::new();
        static_registrations.insert(static_url.clone(), static_id.clone());

        let oidc_metadata = mock_metadata("Example".to_owned());

        let registrations =
            OidcRegistrations::new(base_path, &oidc_metadata, static_registrations).unwrap();

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
        let base_path = dir.path().to_str().unwrap();

        let static_url = Url::parse("https://example.com").unwrap();
        let static_id = ClientId("static_client_id".to_owned());
        let dynamic_url = Url::parse("https://example.org").unwrap();
        let dynamic_id = ClientId("dynamic_client_id".to_owned());

        let oidc_metadata = mock_metadata("Example".to_owned());

        let mut static_registrations = HashMap::new();
        static_registrations.insert(static_url.clone(), static_id.clone());

        let registrations =
            OidcRegistrations::new(base_path, &oidc_metadata, static_registrations.clone())
                .unwrap();
        registrations.set_and_write_client_id(dynamic_id.clone(), dynamic_url.clone()).unwrap();

        assert_eq!(registrations.client_id(&static_url), Some(static_id.clone()));
        assert_eq!(registrations.client_id(&dynamic_url), Some(dynamic_id));

        // When the app name changes.
        let new_oidc_metadata = mock_metadata("New App".to_owned());

        let registrations =
            OidcRegistrations::new(base_path, &new_oidc_metadata, static_registrations).unwrap();

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
