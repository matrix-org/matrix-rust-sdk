use std::sync::Arc;

use matrix_sdk_base::crypto::{
    store::{LockableCryptoStore, Store},
    CryptoStoreError,
};
use matrix_sdk_common::store_locks::{
    CrossProcessStoreLock, CrossProcessStoreLockGuard, LockStoreError,
};
use thiserror::Error;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tracing::trace;

use super::OidcSessionTokens;
use crate::oidc::hash;

/// Key in the database for the custom value holding the current session tokens
/// hash.
const OIDC_SESSION_HASH_KEY: &str = "oidc_session_hash";

/// Newtype to identify that a value is a session tokens' hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SessionHash(u64);

/// Compute a hash uniquely identifying the OIDC session tokens.
fn compute_session_hash(tokens: &OidcSessionTokens) -> SessionHash {
    // Subset of `SessionTokens` fit for hashing
    #[derive(Hash)]
    struct HashableSessionTokens<'a> {
        access_token: &'a str,
        refresh_token: Option<&'a str>,
    }

    SessionHash(hash(&HashableSessionTokens {
        access_token: &tokens.access_token,
        refresh_token: tokens.refresh_token.as_deref(),
    }))
}

#[derive(Clone)]
pub(super) struct CrossProcessRefreshManager {
    store: Store,
    store_lock: CrossProcessStoreLock<LockableCryptoStore>,
    known_session_hash: Arc<Mutex<Option<SessionHash>>>,
}

impl CrossProcessRefreshManager {
    /// Create a new `CrossProcessRefreshManager`.
    pub fn new(store: Store, lock: CrossProcessStoreLock<LockableCryptoStore>) -> Self {
        Self { store, store_lock: lock, known_session_hash: Arc::new(Mutex::new(None)) }
    }

    /// Returns the value in the database representing the lock holder.
    pub fn lock_holder(&self) -> &str {
        self.store_lock.lock_holder()
    }

    /// Wait for up to 60 seconds to get a cross-process store lock, then either
    /// timeout (as an error) or return a lock guard.
    ///
    /// The guard also contains information useful to react upon another
    /// background refresh having happened in the database already.
    pub async fn spin_lock(
        &self,
    ) -> Result<CrossProcessRefreshLockGuard, CrossProcessRefreshLockError> {
        // Acquire the intra-process mutex, to avoid multiple requests across threads in
        // the current process.
        trace!("Waiting for intra-process lock...");
        let prev_hash = self.known_session_hash.clone().lock_owned().await;

        // Acquire the cross-process mutex, to avoid multiple requests across different
        // processus.
        trace!("Waiting for inter-process lock...");
        let store_guard = self.store_lock.spin_lock(Some(60000)).await?;

        // Read the previous session hash in the database.
        let current_db_session_bytes = self.store.get_custom_value(OIDC_SESSION_HASH_KEY).await?;

        let db_hash = if let Some(val) = current_db_session_bytes {
            Some(u64::from_le_bytes(
                val.try_into().map_err(|_| CrossProcessRefreshLockError::InvalidPreviousHash)?,
            ))
        } else {
            None
        };

        let db_hash = db_hash.map(SessionHash);

        let hash_mismatch = match (db_hash, *prev_hash) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(db), Some(known)) => db != known,
        };

        trace!(
            "Hash mismatch? {:?} (prev. known={:?}, db={:?})",
            hash_mismatch,
            *prev_hash,
            db_hash
        );

        let guard = CrossProcessRefreshLockGuard {
            hash_guard: prev_hash,
            _store_guard: store_guard,
            hash_mismatch,
            db_hash,
            store: self.store.clone(),
        };

        Ok(guard)
    }

    pub async fn restore_session(&self, tokens: &OidcSessionTokens) {
        let prev_tokens_hash = compute_session_hash(tokens);
        *self.known_session_hash.lock().await = Some(prev_tokens_hash);
    }

    pub async fn on_logout(&self) -> Result<(), CrossProcessRefreshLockError> {
        self.store
            .remove_custom_value(OIDC_SESSION_HASH_KEY)
            .await
            .map_err(CrossProcessRefreshLockError::StoreError)?;
        *self.known_session_hash.lock().await = None;
        Ok(())
    }
}

pub(super) struct CrossProcessRefreshLockGuard {
    /// The hash for the latest session, either the one we knew, or the latest
    /// one read from the database, if it was more up to date.
    hash_guard: OwnedMutexGuard<Option<SessionHash>>,

    /// Cross-process lock being hold.
    _store_guard: CrossProcessStoreLockGuard,

    /// Reference to the underlying store, for storing the hash of the latest
    /// known session (as a custom value).
    store: Store,

    /// Do the in-memory hash and database hash mismatch?
    ///
    /// If so, this indicates that another process may have refreshed the token
    /// in the background.
    ///
    /// We don't consider it a mismatch if there was no previous value in the
    /// database. We do consider it a mismatch if there was no in-memory
    /// value known, but one was known in the database.
    pub hash_mismatch: bool,

    /// Session hash previously stored in the DB.
    ///
    /// Used for debugging and testing purposes.
    db_hash: Option<SessionHash>,
}

impl CrossProcessRefreshLockGuard {
    /// Updates the `SessionTokens` hash in-memory only.
    fn save_in_memory(&mut self, hash: SessionHash) {
        *self.hash_guard = Some(hash);
    }

    /// Updates the `SessionTokens` hash in the database only.
    async fn save_in_database(
        &self,
        hash: SessionHash,
    ) -> Result<(), CrossProcessRefreshLockError> {
        self.store.set_custom_value(OIDC_SESSION_HASH_KEY, hash.0.to_le_bytes().to_vec()).await?;
        Ok(())
    }

    /// Updates the `SessionTokens` hash in both memory and database.
    ///
    /// Must be called after a successful refresh.
    pub async fn save_in_memory_and_db(
        &mut self,
        tokens: &OidcSessionTokens,
    ) -> Result<(), CrossProcessRefreshLockError> {
        let hash = compute_session_hash(tokens);
        self.save_in_memory(hash);
        self.save_in_database(hash).await?;
        Ok(())
    }

    /// Handle a mismatch by making sure values in the database and memory match
    /// tokens we trust.
    pub async fn handle_mismatch(
        &mut self,
        trusted_tokens: &OidcSessionTokens,
    ) -> Result<(), CrossProcessRefreshLockError> {
        let new_hash = compute_session_hash(trusted_tokens);
        trace!("Trusted OIDC tokens have hash {new_hash:?}; db had {:?}", self.db_hash);

        if let Some(db_hash) = &self.db_hash {
            if new_hash != *db_hash {
                // That should never happen, unless we got into an impossible situation!
                // In this case, we assume the value returned by the callback is always
                // correct, so override that in the database too.
                tracing::error!("error: DB and trusted disagree. Overriding in DB.");
                self.save_in_database(new_hash).await?;
            }
        }

        self.save_in_memory(new_hash);
        Ok(())
    }
}

/// An error that happened when interacting with the cross-process store lock
/// during a token refresh.
#[derive(Debug, Error)]
pub enum CrossProcessRefreshLockError {
    /// Underlying error caused by the store.
    #[error(transparent)]
    StoreError(#[from] CryptoStoreError),

    /// The locking itself failed.
    #[error(transparent)]
    LockError(#[from] LockStoreError),

    /// The previous hash isn't valid.
    #[error("the previous stored hash isn't a valid integer")]
    InvalidPreviousHash,

    /// The lock hasn't been set up.
    #[error("the cross-process lock hasn't been set up with `enable_cross_process_refresh_lock")]
    MissingLock,

    /// Cross-process lock was set, but without session callbacks.
    #[error("reload session callback must be set with Oidc::set_callbacks() for the cross-process lock to work")]
    MissingReloadSession,

    /// Session tokens returned by the reload_session callback were not for
    /// OIDC.
    #[error("session tokens returned by the reload_session callback were not for OIDC")]
    InvalidSessionTokens,

    /// The store has been created twice.
    #[error(
        "the cross-process lock has been set up twice with `enable_cross_process_refresh_lock`"
    )]
    DuplicatedLock,
}

#[cfg(test)]
mod tests {
    use mas_oidc_client::types::{
        client_credentials::ClientCredentials, iana::oauth::OAuthClientAuthenticationMethod,
        registration::ClientMetadata,
    };
    use matrix_sdk_base::SessionMeta;
    use matrix_sdk_test::async_test;
    use ruma::api::client::discovery::discover_homeserver::AuthenticationServerInfo;

    use super::compute_session_hash;
    use crate::{
        oidc::{OidcSession, OidcSessionTokens, UserSession},
        test_utils::test_client_builder,
        Error,
    };

    fn fake_session(tokens: OidcSessionTokens) -> OidcSession {
        OidcSession {
            credentials: ClientCredentials::None { client_id: "test_client_id".to_owned() },
            metadata: ClientMetadata {
                redirect_uris: Some(vec![]), // empty vector is ok lol
                token_endpoint_auth_method: Some(OAuthClientAuthenticationMethod::None),
                ..ClientMetadata::default()
            }
            .validate()
            .expect("validate client metadata"),
            user: UserSession {
                meta: SessionMeta {
                    user_id: ruma::user_id!("@u:e.uk").to_owned(),
                    device_id: ruma::device_id!("XYZ").to_owned(),
                },
                tokens,
                issuer_info: AuthenticationServerInfo::new("issuer".to_owned(), None),
            },
        }
    }

    #[cfg(feature = "e2e-encryption")]
    #[async_test]
    async fn test_oidc_restore_session_lock() -> Result<(), Error> {
        // Create a client that will use sqlite databases.

        let tmp_dir = tempfile::tempdir()?;
        let client = test_client_builder(None).sqlite_store(tmp_dir, None).build().await.unwrap();

        let tokens = OidcSessionTokens {
            access_token: "prev-access-token".to_owned(),
            refresh_token: Some("prev-refresh-token".to_owned()),
            latest_id_token: None,
        };

        client.oidc().enable_cross_process_refresh_lock("test".to_owned()).await?;

        client.set_session_callbacks(
            Box::new({
                // This is only called because of extra checks in the code.
                let tokens = tokens.clone();
                move |_| Ok(crate::authentication::SessionTokens::Oidc(tokens.clone()))
            }),
            Box::new(|_| panic!("save_session_callback shouldn't be called here")),
        )?;

        let session_hash = compute_session_hash(&tokens);
        client.oidc().restore_session(fake_session(tokens.clone())).await?;

        assert_eq!(client.oidc().session_tokens().unwrap(), tokens);

        let oidc = client.oidc();
        let xp_manager = oidc.ctx().cross_process_token_refresh_manager.get().unwrap();

        {
            let known_session = xp_manager.known_session_hash.lock().await;
            assert_eq!(known_session.unwrap(), session_hash);
        }

        {
            let lock = xp_manager.spin_lock().await.unwrap();
            assert!(!lock.hash_mismatch);
            assert_eq!(lock.db_hash.unwrap(), session_hash);
        }

        Ok(())
    }
}
