use std::sync::Arc;

use matrix_sdk_base::crypto::{
    store::{LockableCryptoStore, Store},
    CryptoStoreError,
};
use matrix_sdk_common::store_locks::{
    CrossProcessStoreLock, CrossProcessStoreLockGuard, LockStoreError,
};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tracing::trace;

use super::OidcSessionTokens;

/// Key in the database for the custom value holding the current session tokens
/// hash.
const OIDC_SESSION_HASH_KEY: &str = "oidc_session_hash";

/// Newtype to identify that a value is a session tokens' hash.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionHash(Vec<u8>);

/// Compute a hash uniquely identifying the OIDC session tokens.
fn compute_session_hash(tokens: &OidcSessionTokens) -> SessionHash {
    let mut hash = Sha256::new().chain_update(tokens.access_token.as_bytes());
    if let Some(refresh_token) = &tokens.refresh_token {
        hash = hash.chain_update(refresh_token.as_bytes());
    }
    SessionHash(hash.finalize().to_vec())
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

        let db_hash = current_db_session_bytes.map(SessionHash);

        let hash_mismatch = match (&db_hash, &*prev_hash) {
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
        hash: &SessionHash,
    ) -> Result<(), CrossProcessRefreshLockError> {
        self.store.set_custom_value(OIDC_SESSION_HASH_KEY, hash.0.clone()).await?;
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
        self.save_in_database(&hash).await?;
        self.save_in_memory(hash);
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
                self.save_in_database(&new_hash).await?;
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
    #[error("reload session callback must be set with Client::set_session_callbacks() for the cross-process lock to work")]
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

#[cfg(all(test, feature = "e2e-encryption"))]
mod tests {
    use std::sync::Arc;

    use anyhow::Context as _;
    use futures_util::future::join_all;
    use matrix_sdk_base::SessionMeta;
    use matrix_sdk_test::async_test;
    use ruma::{
        api::client::discovery::discover_homeserver::AuthenticationServerInfo, owned_device_id,
        owned_user_id,
    };
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::compute_session_hash;
    use crate::{
        oidc::{
            backend::mock::{MockImpl, ISSUER_URL},
            tests,
            tests::mock_registered_client_data,
            Oidc, OidcSessionTokens,
        },
        test_utils::test_client_builder,
        Error,
    };

    #[async_test]
    async fn test_restore_session_lock() -> Result<(), Error> {
        // Create a client that will use sqlite databases.

        let tmp_dir = tempfile::tempdir()?;
        let client = test_client_builder(Some("https://example.org".to_owned()))
            .sqlite_store(&tmp_dir, None)
            .build()
            .await
            .unwrap();

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
        client.oidc().restore_session(tests::mock_session(tokens.clone())).await?;

        assert_eq!(client.oidc().session_tokens().unwrap(), tokens);

        let oidc = client.oidc();
        let xp_manager = oidc.ctx().cross_process_token_refresh_manager.get().unwrap();

        {
            let known_session = xp_manager.known_session_hash.lock().await;
            assert_eq!(known_session.as_ref().unwrap(), &session_hash);
        }

        {
            let lock = xp_manager.spin_lock().await.unwrap();
            assert!(!lock.hash_mismatch);
            assert_eq!(lock.db_hash.unwrap(), session_hash);
        }

        Ok(())
    }

    #[async_test]
    async fn test_finish_login() -> anyhow::Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/r0/account/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_id": "@joe:example.org",
                "device_id": "D3V1C31D",
            })))
            .expect(1)
            .named("`GET /whoami` good token")
            .mount(&server)
            .await;

        let tmp_dir = tempfile::tempdir()?;
        let client =
            test_client_builder(Some(server.uri())).sqlite_store(&tmp_dir, None).build().await?;

        let oidc = Oidc { client: client.clone(), backend: Arc::new(MockImpl::new()) };

        // Restore registered client.
        let issuer_info = AuthenticationServerInfo::new(ISSUER_URL.to_owned(), None);
        let (client_credentials, client_metadata) = mock_registered_client_data();
        oidc.restore_registered_client(issuer_info, client_metadata, client_credentials);

        // Enable cross-process lock.
        oidc.enable_cross_process_refresh_lock("lock".to_owned()).await?;

        // Simulate we've done finalize_authorization / restore_session before.
        let session_tokens = OidcSessionTokens {
            access_token: "access".to_owned(),
            refresh_token: Some("refresh".to_owned()),
            latest_id_token: None,
        };
        oidc.set_session_tokens(session_tokens.clone());

        // Now, finishing logging will get the user and device ids.
        oidc.finish_login().await?;

        let session_meta = client.session_meta().context("should have session meta now")?;
        assert_eq!(
            *session_meta,
            SessionMeta {
                user_id: owned_user_id!("@joe:example.org"),
                device_id: owned_device_id!("D3V1C31D")
            }
        );

        {
            // The cross process lock has been correctly updated, and the next attempt to
            // take it won't result in a mismatch.
            let xp_manager =
                oidc.ctx().cross_process_token_refresh_manager.get().context("must have lock")?;
            let guard = xp_manager.spin_lock().await?;
            let actual_hash = compute_session_hash(&session_tokens);
            assert_eq!(guard.db_hash.as_ref(), Some(&actual_hash));
            assert_eq!(guard.hash_guard.as_ref(), Some(&actual_hash));
            assert!(!guard.hash_mismatch);
        }

        Ok(())
    }

    #[async_test]
    async fn test_refresh_access_token_twice() -> anyhow::Result<()> {
        // This tests that refresh token works, and that it doesn't cause multiple token
        // refreshes whenever one spawns two refreshes around the same time.

        let tmp_dir = tempfile::tempdir()?;
        let client = test_client_builder(Some("https://example.org".to_owned()))
            .sqlite_store(&tmp_dir, None)
            .build()
            .await?;

        let prev_tokens = OidcSessionTokens {
            access_token: "prev-access-token".to_owned(),
            refresh_token: Some("prev-refresh-token".to_owned()),
            latest_id_token: None,
        };

        let next_tokens = OidcSessionTokens {
            access_token: "next-access-token".to_owned(),
            refresh_token: Some("next-refresh-token".to_owned()),
            latest_id_token: None,
        };

        let backend = Arc::new(
            MockImpl::new()
                .next_session_tokens(next_tokens.clone())
                .expected_refresh_token(prev_tokens.refresh_token.clone().unwrap()),
        );
        let oidc = Oidc { client: client.clone(), backend: backend.clone() };

        // Enable cross-process lock.
        oidc.enable_cross_process_refresh_lock("lock".to_owned()).await?;

        // Restore the session.
        oidc.restore_session(tests::mock_session(prev_tokens.clone())).await?;

        // Immediately try to refresh the access token twice in parallel.
        for result in join_all([oidc.refresh_access_token(), oidc.refresh_access_token()]).await {
            result?;
        }

        // There should have been at most one refresh.
        assert_eq!(*backend.num_refreshes.lock().unwrap(), 1);

        {
            // The cross process lock has been correctly updated, and the next attempt to
            // take it won't result in a mismatch.
            let xp_manager =
                oidc.ctx().cross_process_token_refresh_manager.get().context("must have lock")?;
            let guard = xp_manager.spin_lock().await?;
            let actual_hash = compute_session_hash(&next_tokens);
            assert_eq!(guard.db_hash.as_ref(), Some(&actual_hash));
            assert_eq!(guard.hash_guard.as_ref(), Some(&actual_hash));
            assert!(!guard.hash_mismatch);
        }

        Ok(())
    }

    #[async_test]
    async fn test_cross_process_concurrent_refresh() -> anyhow::Result<()> {
        // Create the backend.
        let prev_tokens = OidcSessionTokens {
            access_token: "prev-access-token".to_owned(),
            refresh_token: Some("prev-refresh-token".to_owned()),
            latest_id_token: None,
        };

        let next_tokens = OidcSessionTokens {
            access_token: "next-access-token".to_owned(),
            refresh_token: Some("next-refresh-token".to_owned()),
            latest_id_token: None,
        };

        let backend = Arc::new(
            MockImpl::new()
                .next_session_tokens(next_tokens.clone())
                .expected_refresh_token(prev_tokens.refresh_token.clone().unwrap()),
        );

        // Create the first client.
        let tmp_dir = tempfile::tempdir()?;
        let client = test_client_builder(Some("https://example.org".to_owned()))
            .sqlite_store(&tmp_dir, None)
            .build()
            .await?;

        let oidc = Oidc { client: client.clone(), backend: backend.clone() };
        oidc.enable_cross_process_refresh_lock("client1".to_owned()).await?;
        oidc.restore_session(tests::mock_session(prev_tokens.clone())).await?;

        // Create a second client, without restoring it, to test that a token update
        // before restoration doesn't cause new issues.
        let unrestored_client = test_client_builder(Some("https://example.org".to_owned()))
            .sqlite_store(&tmp_dir, None)
            .build()
            .await?;
        let unrestored_oidc = Oidc { client: unrestored_client.clone(), backend: backend.clone() };
        unrestored_oidc.enable_cross_process_refresh_lock("unrestored_client".to_owned()).await?;

        {
            // Create a third client that will run a refresh while the others two are doing
            // nothing.
            let client3 = test_client_builder(Some("https://example.org".to_owned()))
                .sqlite_store(&tmp_dir, None)
                .build()
                .await?;

            let oidc3 = Oidc { client: client3.clone(), backend: backend.clone() };
            oidc3.enable_cross_process_refresh_lock("client3".to_owned()).await?;
            oidc3.restore_session(tests::mock_session(prev_tokens.clone())).await?;

            // Run a refresh in the second client; this will invalidate the tokens from the
            // first token.
            oidc3.refresh_access_token().await?;

            assert_eq!(oidc3.session_tokens(), Some(next_tokens.clone()));

            // Reading from the cross-process lock for the second client only shows the new
            // tokens.
            let xp_manager =
                oidc3.ctx().cross_process_token_refresh_manager.get().context("must have lock")?;
            let guard = xp_manager.spin_lock().await?;
            let actual_hash = compute_session_hash(&next_tokens);
            assert_eq!(guard.db_hash.as_ref(), Some(&actual_hash));
            assert_eq!(guard.hash_guard.as_ref(), Some(&actual_hash));
            assert!(!guard.hash_mismatch);
        }

        {
            // Restoring the client that was not restored yet will work Just Fine.
            let oidc = unrestored_oidc;

            unrestored_client.set_session_callbacks(
                Box::new({
                    // This is only called because of extra checks in the code.
                    let tokens = next_tokens.clone();
                    move |_| Ok(crate::authentication::SessionTokens::Oidc(tokens.clone()))
                }),
                Box::new(|_| panic!("save_session_callback shouldn't be called here")),
            )?;

            oidc.restore_session(tests::mock_session(prev_tokens.clone())).await?;

            // And this client is now aware of the latest tokens.
            let xp_manager =
                oidc.ctx().cross_process_token_refresh_manager.get().context("must have lock")?;
            let guard = xp_manager.spin_lock().await?;
            let next_hash = compute_session_hash(&next_tokens);
            assert_eq!(guard.db_hash.as_ref(), Some(&next_hash));
            assert_eq!(guard.hash_guard.as_ref(), Some(&next_hash));
            assert!(!guard.hash_mismatch);

            drop(oidc);
            drop(unrestored_client);
        }

        {
            // The cross process lock has been correctly updated, and the next attempt to
            // take it will result in a mismatch.
            let xp_manager =
                oidc.ctx().cross_process_token_refresh_manager.get().context("must have lock")?;
            let guard = xp_manager.spin_lock().await?;
            let previous_hash = compute_session_hash(&prev_tokens);
            let next_hash = compute_session_hash(&next_tokens);
            assert_eq!(guard.db_hash, Some(next_hash));
            assert_eq!(guard.hash_guard.as_ref(), Some(&previous_hash));
            assert!(guard.hash_mismatch);
        }

        client.set_session_callbacks(
            Box::new({
                // This is only called because of extra checks in the code.
                let tokens = next_tokens.clone();
                move |_| Ok(crate::authentication::SessionTokens::Oidc(tokens.clone()))
            }),
            Box::new(|_| panic!("save_session_callback shouldn't be called here")),
        )?;

        oidc.refresh_access_token().await?;

        {
            // The next attempt to take the lock isn't a mismatch.
            let xp_manager =
                oidc.ctx().cross_process_token_refresh_manager.get().context("must have lock")?;
            let guard = xp_manager.spin_lock().await?;
            let actual_hash = compute_session_hash(&next_tokens);
            assert_eq!(guard.db_hash.as_ref(), Some(&actual_hash));
            assert_eq!(guard.hash_guard.as_ref(), Some(&actual_hash));
            assert!(!guard.hash_mismatch);
        }

        // There should have been at most one refresh.
        assert_eq!(*backend.num_refreshes.lock().unwrap(), 1);

        Ok(())
    }

    #[async_test]
    async fn test_logout() -> anyhow::Result<()> {
        let tmp_dir = tempfile::tempdir()?;
        let client = test_client_builder(Some("https://example.org".to_owned()))
            .sqlite_store(&tmp_dir, None)
            .build()
            .await?;

        let tokens = OidcSessionTokens {
            access_token: "prev-access-token".to_owned(),
            refresh_token: Some("prev-refresh-token".to_owned()),
            latest_id_token: None,
        };

        let backend = Arc::new(MockImpl::new());
        let oidc = Oidc { client: client.clone(), backend: backend.clone() };

        // Enable cross-process lock.
        oidc.enable_cross_process_refresh_lock("lock".to_owned()).await?;

        // Restore the session.
        oidc.restore_session(tests::mock_session(tokens.clone())).await?;

        let end_session_builder = oidc.logout().await?;

        // No end session builder because our test impl doesn't provide an end session
        // endpoint.
        assert!(end_session_builder.is_none());

        // Both the access token and the refresh tokens have been invalidated.
        {
            let revoked = backend.revoked_tokens.lock().unwrap();
            assert_eq!(revoked.len(), 2);
            assert_eq!(
                *revoked,
                vec![tokens.access_token.clone(), tokens.refresh_token.clone().unwrap(),]
            );
        }

        {
            // The cross process lock has been correctly updated, and all the hashes are
            // empty after a logout.
            let xp_manager =
                oidc.ctx().cross_process_token_refresh_manager.get().context("must have lock")?;
            let guard = xp_manager.spin_lock().await?;
            assert!(guard.db_hash.is_none());
            assert!(guard.hash_guard.is_none());
            assert!(!guard.hash_mismatch);
        }

        Ok(())
    }
}
