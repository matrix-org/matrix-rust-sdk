// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use olm_rs::account::OlmAccount;
use std::collections::{hash_map::Iter, hash_map::Keys, hash_map::Values, HashMap};

use serde;
use serde::Deserialize;

/// Struct representing the parsed result of `OlmAccount::identity_keys()`.
#[derive(Deserialize, Debug, PartialEq)]
pub struct IdentityKeys {
    #[serde(flatten)]
    keys: HashMap<String, String>,
}

impl IdentityKeys {
    /// Get the public part of the ed25519 key of the account.
    pub fn ed25519(&self) -> &str {
        &self.keys["ed25519"]
    }

    /// Get the public part of the curve25519 key of the account.
    pub fn curve25519(&self) -> &str {
        &self.keys["curve25519"]
    }

    /// Get a reference to the key of the given key type.
    pub fn get(&self, key_type: &str) -> Option<&str> {
        let ret = self.keys.get(key_type);
        ret.map(|x| &**x)
    }

    /// An iterator visiting all public keys of the account.
    pub fn values(&self) -> Values<String, String> {
        self.keys.values()
    }

    /// An iterator visiting all key types of the account.
    pub fn keys(&self) -> Keys<String, String> {
        self.keys.keys()
    }

    /// An iterator visiting all key-type, key pairs of the account.
    pub fn iter(&self) -> Iter<String, String> {
        self.keys.iter()
    }

    /// Returns true if the account contains a key with the given key type.
    pub fn contains_key(&self, key_type: &str) -> bool {
        self.keys.contains_key(key_type)
    }
}

#[derive(Deserialize, Debug, PartialEq)]
/// Struct representing the the one-time keys.
/// The keys can be accessed in a map-like fashion.
pub struct OneTimeKeys {
    #[serde(flatten)]
    keys: HashMap<String, HashMap<String, String>>,
}

impl OneTimeKeys {
    /// Get the HashMap containing the curve25519 one-time keys.
    /// This is the same as using `get("curve25519").unwrap()`
    pub fn curve25519(&self) -> &HashMap<String, String> {
        &self.keys["curve25519"]
    }

    /// Get a reference to the hashmap corresponding to given key type.
    pub fn get(&self, key_type: &str) -> Option<&HashMap<String, String>> {
        self.keys.get(key_type)
    }

    /// An iterator visiting all one-time key hashmaps in an arbitrary order.
    pub fn values(&self) -> Values<String, HashMap<String, String>> {
        self.keys.values()
    }

    /// An iterator visiting all one-time key types in an arbitrary order.
    pub fn keys(&self) -> Keys<String, HashMap<String, String>> {
        self.keys.keys()
    }

    /// An iterator visiting all one-time key types and their respective
    /// key hashmaps in an arbitrary order.
    pub fn iter(&self) -> Iter<String, HashMap<String, String>> {
        self.keys.iter()
    }

    /// Returns `true` if the struct contains the given key type.
    /// This does not mean that there are any keys for the given key type.
    pub fn contains_key(&self, key_type: &str) -> bool {
        self.keys.contains_key(key_type)
    }
}

pub struct Account {
    inner: OlmAccount,
    pub(crate) shared: bool,
}

impl Account {
    /// Create a new account.
    pub fn new() -> Self {
        Account {
            inner: OlmAccount::new(),
            shared: false,
        }
    }

    /// Get the public parts of the identity keys for the account.
    pub fn identity_keys(&self) -> IdentityKeys {
        serde_json::from_str(&self.inner.identity_keys()).expect("Can't parse the identity keys")
    }

    /// Has the account been shared with the server.
    pub fn shared(&self) -> bool {
        self.shared
    }

    /// Get the one-time keys of the account.
    ///
    /// This can be empty, keys need to be generated first.
    pub fn one_time_keys(&self) -> OneTimeKeys {
        serde_json::from_str(&self.inner.one_time_keys()).expect("Can't parse the one-time keys")
    }

    /// Generate count number of one-time keys.
    pub fn generate_one_time_keys(&self, count: usize) {
        self.inner.generate_one_time_keys(count);
    }

    /// Get the maximum number of one-time keys the account can hold.
    pub fn max_one_time_keys(&self) -> usize {
        self.inner.max_number_of_one_time_keys()
    }

    /// Mark the current set of one-time keys as being published.
    pub fn mark_keys_as_published(&self) {
        self.inner.mark_keys_as_published();
    }

    pub fn sign(&self, string: &str) -> String {
        self.inner.sign(string)
    }
}

#[cfg(test)]
mod test {
    use crate::crypto::olm::Account;

    #[test]
    fn account_creation() {
        let account = Account::new();
        let identyty_keys = account.identity_keys();

        assert!(!account.shared());
        assert!(!identyty_keys.ed25519().is_empty());
        assert_ne!(identyty_keys.values().len(), 0);
        assert_ne!(identyty_keys.keys().len(), 0);
        assert_ne!(identyty_keys.iter().len(), 0);
        assert!(identyty_keys.contains_key("ed25519"));
        assert_eq!(
            identyty_keys.ed25519(),
            identyty_keys.get("ed25519").unwrap()
        );
        assert!(!identyty_keys.curve25519().is_empty());
    }

    #[test]
    fn one_time_keys_creation() {
        let account = Account::new();
        let one_time_keys = account.one_time_keys();

        assert!(one_time_keys.curve25519().is_empty());
        assert_ne!(account.max_one_time_keys(), 0);

        account.generate_one_time_keys(10);
        let one_time_keys = account.one_time_keys();

        assert!(!one_time_keys.curve25519().is_empty());
        assert_ne!(one_time_keys.values().len(), 0);
        assert_ne!(one_time_keys.keys().len(), 0);
        assert_ne!(one_time_keys.iter().len(), 0);
        assert!(one_time_keys.contains_key("curve25519"));
        assert_eq!(one_time_keys.curve25519().keys().len(), 10);
        assert_eq!(
            one_time_keys.curve25519(),
            one_time_keys.get("curve25519").unwrap()
        );

        account.mark_keys_as_published();
        let one_time_keys = account.one_time_keys();
        assert!(one_time_keys.curve25519().is_empty());
    }
}
