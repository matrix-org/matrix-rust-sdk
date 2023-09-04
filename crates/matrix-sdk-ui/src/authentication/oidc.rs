use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    fs,
    fs::File,
    hash::{Hash, Hasher},
    io::{BufReader, BufWriter},
    path::PathBuf,
};

use matrix_sdk::oidc::types::registration::VerifiedClientMetadata;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum OidcRegistrationsError {
    #[error("Failed to use the supplied base path.")]
    InvalidBasePath,
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
    static_registrations: HashMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RegistrationData {
    /// The hash for the metadata used to register the client.
    metadata_hash: u64,
    /// All of the registrations this client has made as a HashMap of issuer URL
    /// (as a string) to client ID (as a string).
    dynamic_registrations: HashMap<String, String>,
}

/// Manages the storage of OIDC registrations.
impl OidcRegistrations {
    pub fn new(
        base_path: &str,
        metadata: &VerifiedClientMetadata,
        static_registrations: HashMap<String, String>,
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
    fn registration_data(&self) -> RegistrationData {
        let reader = match File::open(&self.file_path) {
            Ok(file) => BufReader::new(file),
            Err(e) => {
                tracing::warn!("Failed to open registrations file: {e}");
                return RegistrationData {
                    metadata_hash: self.metadata_hash,
                    dynamic_registrations: Default::default(),
                };
            }
        };

        let registration_data = match serde_json::from_reader::<_, RegistrationData>(reader) {
            Ok(data) => data,
            Err(e) => {
                tracing::error!("Failed to parse registrations file: {e}");
                return RegistrationData {
                    metadata_hash: self.metadata_hash,
                    dynamic_registrations: Default::default(),
                };
            }
        };

        if registration_data.metadata_hash != self.metadata_hash {
            tracing::warn!("Metadata hash mismatch, clearing registrations");
            return RegistrationData {
                metadata_hash: self.metadata_hash,
                dynamic_registrations: Default::default(),
            };
        }

        registration_data
    }

    /// Returns the client ID registered for a particular issuer or None if a
    /// registration hasn't been made.
    pub fn client_id(&self, issuer: &str) -> Option<String> {
        let mut data = self.registration_data();
        data.dynamic_registrations.extend(self.static_registrations.clone());
        data.dynamic_registrations.get(issuer).cloned()
    }

    /// Stores a new client ID registration for a particular issuer. If a client
    /// ID has already been stored, this will overwrite the old value.
    pub fn set_client_id(
        &self,
        client_id: String,
        issuer: String,
    ) -> Result<(), OidcRegistrationsError> {
        let mut data = self.registration_data();
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

        let mut static_registrations = HashMap::new();
        static_registrations
            .insert("https://example.com".to_owned(), "static_client_id".to_owned());

        let oidc_metadata = mock_metadata("Example".to_owned());

        let registrations =
            OidcRegistrations::new(base_path, &oidc_metadata, static_registrations).unwrap();

        assert_eq!(
            registrations.client_id("https://example.com"),
            Some("static_client_id".to_owned())
        );
        assert_eq!(registrations.client_id("https://example.org"), None);

        // When a dynamic registration is added.
        registrations
            .set_client_id("dynamic_client_id".to_owned(), "https://example.org".to_owned())
            .unwrap();

        // Then the dynamic registration should be stored and the static registration
        // should be unaffected.
        assert_eq!(
            registrations.client_id("https://example.com"),
            Some("static_client_id".to_owned())
        );
        assert_eq!(
            registrations.client_id("https://example.org"),
            Some("dynamic_client_id".to_owned())
        );
    }

    #[test]
    fn test_change_of_metadata() {
        // Given a single registration with an example app name.
        let dir = tempdir().unwrap();
        let base_path = dir.path().to_str().unwrap();

        let oidc_metadata = mock_metadata("Example".to_owned());

        let registrations =
            OidcRegistrations::new(base_path, &oidc_metadata, HashMap::new()).unwrap();
        registrations
            .set_client_id("dynamic_client_id".to_owned(), "https://example.org".to_owned())
            .unwrap();

        assert_eq!(
            registrations.client_id("https://example.org"),
            Some("dynamic_client_id".to_owned())
        );

        // When the app name changes.
        let new_oidc_metadata = mock_metadata("New App".to_owned());

        let registrations =
            OidcRegistrations::new(base_path, &new_oidc_metadata, HashMap::new()).unwrap();

        // Then the registration is cleared.
        assert_eq!(registrations.client_id("https://example.org"), None);
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
