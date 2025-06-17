use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Data representing an Olm account.
#[derive(Debug, Serialize, Deserialize)]
pub struct OlmAccountData {
    /// Our Curve25519 identity public key.
    pub identity_key: String,
    /// Our Ed25519 identity public key.
    pub signing_key: String,
    /// A map of one-time Curve25519 public keys, identified by their ID.
    pub one_time_keys: BTreeMap<String, String>,
}

impl OlmAccountData {
    /// Create a new `OlmAccountData` instance.
    pub fn new(
        identity_key: String,
        signing_key: String,
        one_time_keys: BTreeMap<String, String>,
    ) -> Self {
        Self {
            identity_key,
            signing_key,
            one_time_keys,
        }
    }
}
