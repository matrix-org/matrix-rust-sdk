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

use std::fmt;
use std::time::Instant;

use olm_rs::account::{IdentityKeys, OlmAccount, OneTimeKeys};
use olm_rs::errors::{OlmAccountError, OlmSessionError};
use olm_rs::session::{OlmMessage, OlmSession, PreKeyMessage};
use olm_rs::PicklingMode;

pub struct Account {
    inner: OlmAccount,
    pub(crate) shared: bool,
}

impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Olm Account: {:?}, shared: {}",
            self.identity_keys(),
            self.shared
        )
    }
}

/// Marking this as Send is safe because nothing will modify the pointer under
/// us from the C side. Sync on the other hand is unsafe since libolm doesn't do
/// any synchronization. We're wrapping the whole Olm machine inside a Mutex to
/// get Sync for it
unsafe impl Send for Account {}
unsafe impl Send for Session {}

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
        self.inner.parsed_identity_keys()
    }

    /// Has the account been shared with the server.
    pub fn shared(&self) -> bool {
        self.shared
    }

    /// Get the one-time keys of the account.
    ///
    /// This can be empty, keys need to be generated first.
    pub fn one_time_keys(&self) -> OneTimeKeys {
        self.inner.parsed_one_time_keys()
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

    pub fn pickle(&self, pickling_mode: PicklingMode) -> String {
        self.inner.pickle(pickling_mode)
    }

    pub fn from_pickle(
        pickle: String,
        pickling_mode: PicklingMode,
        shared: bool,
    ) -> Result<Self, OlmAccountError> {
        let acc = OlmAccount::unpickle(pickle, pickling_mode)?;
        Ok(Account { inner: acc, shared })
    }

    pub fn create_inbound_session_from(
        &self,
        their_identity_key: &str,
        message: PreKeyMessage,
    ) -> Result<Session, OlmSessionError> {
        let session = self
            .inner
            .create_inbound_session_from(their_identity_key, message)?;

        let now = Instant::now();

        Ok(Session {
            inner: session,
            creation_time: now.clone(),
            last_use_time: now,
        })
    }
}

impl PartialEq for Account {
    fn eq(&self, other: &Self) -> bool {
        self.identity_keys() == other.identity_keys() && self.shared == other.shared
    }
}

pub struct Session {
    inner: OlmSession,
    creation_time: Instant,
    last_use_time: Instant,
}

impl Session {
    pub fn decrypt(&mut self, message: OlmMessage) -> Result<String, OlmSessionError> {
        let plaintext = self.inner.decrypt(message)?;
        self.last_use_time = Instant::now();
        Ok(plaintext)
    }

    pub fn matches(
        &self,
        their_identity_key: &str,
        message: PreKeyMessage,
    ) -> Result<bool, OlmSessionError> {
        self.inner
            .matches_inbound_session_from(their_identity_key, message)
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
