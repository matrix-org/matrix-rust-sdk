use std::sync::Arc;

#[cfg(feature = "e2e-encryption")]
use matrix_sdk_base::crypto::{
    CryptoStoreError,
    store::{LockableCryptoStore, Store},
};
use matrix_sdk_common::cross_process_lock::{
    CrossProcessLock, CrossProcessLockError, CrossProcessLockGuard,
};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tracing::trace;

use crate::SessionTokens;

/// Key in the database for the custom value holding the current session tokens
/// hash.
const OIDC_SESSION_HASH_KEY: &str = "oidc_session_hash";

/// Newtype to identify that a value is a session tokens' hash.
#[derive(Clone, PartialEq, Eq)]
struct SessionHash(Vec<u8>);

impl SessionHash {
    fn to_hex(&self) -> String {
        const CHARS: &[char; 16] =
            &['0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f'];
        let mut res = String::with_capacity(2 * self.0.len() + 2);
        if !self.0.is_empty() {
            res.push('0');
            res.push('x');
        }
        for &c in &self.0 {
            // We don't really care about little vs big endianness, since we only need a
            // stable format, so we pick one: little endian (print high bits
            // first).
            res.push(CHARS[(c >> 4) as usize]);
            res.push(CHARS[(c & 0b1111) as usize]);
        }
        res
    }
}

impl std::fmt::Debug for SessionHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SessionHash").field(&self.to_hex()).finish()
    }
}

/// Compute a hash uniquely identifying the OAuth 2.0 session tokens.
fn compute_session_hash(tokens: &SessionTokens) -> SessionHash {
    let mut hash = Sha256::new().chain_update(tokens.access_token.as_bytes());
    if let Some(refresh_token) = &tokens.refresh_token {
        hash = hash.chain_update(refresh_token.as_bytes());
    }
    SessionHash(hash.finalize().to_vec())
}

#[derive(Clone)]
pub(super) struct CrossProcessRefreshManager {
    store: Store,
    store_lock: CrossProcessLock<LockableCryptoStore>,
    known_session_hash: Arc<Mutex<Option<SessionHash>>>,
}

impl CrossProcessRefreshManager {
    /// Create a new `CrossProcessRefreshManager`.
    pub fn new(store: Store, lock: CrossProcessLock<LockableCryptoStore>) -> Self {
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
        let store_guard = self
            .store_lock
            .spin_lock(Some(60000))
            .await
            .map_err(|err| {
                CrossProcessRefreshLockError::LockError(CrossProcessLockError::TryLock(Box::new(
                    err,
                )))
            })?
            .map_err(|err| CrossProcessRefreshLockError::LockError(err.into()))?;

        // Read the previous session hash in the database.
        let current_db_session_bytes = self.store.get_custom_value(OIDC_SESSION_HASH_KEY).await?;

        let db_hash = current_db_session_bytes.map(SessionHash);

        let hash_mismatch = match (&db_hash, &*prev_hash) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(db), Some(known)) => db != known,
        };

        trace!(hash_mismatch, ?prev_hash, ?db_hash);

        let guard = CrossProcessRefreshLockGuard {
            hash_guard: prev_hash,
            _store_guard: store_guard.into_guard(),
            hash_mismatch,
            db_hash,
            store: self.store.clone(),
        };

        Ok(guard)
    }

    pub async fn restore_session(&self, tokens: &SessionTokens) {
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
    _store_guard: CrossProcessLockGuard,

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
        tokens: &SessionTokens,
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
        trusted_tokens: &SessionTokens,
    ) -> Result<(), CrossProcessRefreshLockError> {
        let new_hash = compute_session_hash(trusted_tokens);
        trace!("Trusted OAuth 2.0 tokens have hash {new_hash:?}; db had {:?}", self.db_hash);

        if let Some(db_hash) = &self.db_hash
            && new_hash != *db_hash
        {
            // That should never happen, unless we got into an impossible situation!
            // In this case, we assume the value returned by the callback is always
            // correct, so override that in the database too.
            tracing::error!("error: DB and trusted disagree. Overriding in DB.");
            self.save_in_database(&new_hash).await?;
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
    LockError(#[from] CrossProcessLockError),

    /// The previous hash isn't valid.
    #[error("the previous stored hash isn't a valid integer")]
    InvalidPreviousHash,

    /// The lock hasn't been set up.
    #[error("the cross-process lock hasn't been set up with `enable_cross_process_refresh_lock")]
    MissingLock,

    /// Cross-process lock was set, but without session callbacks.
    #[error(
        "reload session callback must be set with Client::set_session_callbacks() \
         for the cross-process lock to work"
    )]
    MissingReloadSession,

    /// The store has been created twice.
    #[error(
        "the cross-process lock has been set up twice with `enable_cross_process_refresh_lock`"
    )]
    DuplicatedLock,
}

#[cfg(all(test, feature = "e2e-encryption", feature = "sqlite", not(target_family = "wasm")))]
mod tests {

    use anyhow::Context as _;
    use futures_util::future::join_all;
    use matrix_sdk_base::{SessionMeta, store::RoomLoadSettings};
    use matrix_sdk_test::async_test;
    use ruma::{owned_device_id, owned_user_id};

    use super::compute_session_hash;
    use crate::{
        Error,
        authentication::oauth::cross_process::SessionHash,
        test_utils::{
            client::{
                MockClientBuilder, mock_prev_session_tokens_with_refresh,
                mock_session_tokens_with_refresh, oauth::mock_session,
            },
            mocks::MatrixMockServer,
        },
    };

    #[async_test]
    async fn test_restore_session_lock() -> Result<(), Error> {
        // Create a client that will use sqlite databases.

        let tmp_dir = tempfile::tempdir()?;
        let client = MockClientBuilder::new(None)
            .on_builder(|builder| builder.sqlite_store(&tmp_dir, None))
            .unlogged()
            .build()
            .await;

        let tokens = mock_session_tokens_with_refresh();

        client.oauth().enable_cross_process_refresh_lock("test".to_owned()).await?;

        client.set_session_callbacks(
            Box::new({
                // This is only called because of extra checks in the code.
                let tokens = tokens.clone();
                move |_| Ok(tokens.clone())
            }),
            Box::new(|_| panic!("save_session_callback shouldn't be called here")),
        )?;

        let session_hash = compute_session_hash(&tokens);
        client
            .oauth()
            .restore_session(mock_session(tokens.clone()), RoomLoadSettings::default())
            .await?;

        assert_eq!(client.session_tokens().unwrap(), tokens);

        let oauth = client.oauth();
        let xp_manager = oauth.ctx().cross_process_token_refresh_manager.get().unwrap();

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
        let server = MatrixMockServer::new().await;
        server.mock_who_am_i().ok().expect(1).named("whoami").mount().await;

        let tmp_dir = tempfile::tempdir()?;
        let client = server
            .client_builder()
            .on_builder(|builder| builder.sqlite_store(&tmp_dir, None))
            .registered_with_oauth()
            .build()
            .await;
        let oauth = client.oauth();

        // Enable cross-process lock.
        oauth.enable_cross_process_refresh_lock("lock".to_owned()).await?;

        // Simulate we've done finalize_authorization / restore_session before.
        let session_tokens = mock_session_tokens_with_refresh();
        client.auth_ctx().set_session_tokens(session_tokens.clone());

        // Now, finishing logging will get the user ID.
        oauth.load_session(owned_device_id!("D3V1C31D")).await?;

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
                oauth.ctx().cross_process_token_refresh_manager.get().context("must have lock")?;
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

        let server = MatrixMockServer::new().await;

        let oauth_server = server.oauth();
        oauth_server.mock_server_metadata().ok().expect(1..).named("server_metadata").mount().await;
        oauth_server.mock_token().ok().expect(1).named("token").mount().await;

        let tmp_dir = tempfile::tempdir()?;
        let client = server
            .client_builder()
            .on_builder(|builder| builder.sqlite_store(&tmp_dir, None))
            .unlogged()
            .build()
            .await;
        let oauth = client.oauth();

        let next_tokens = mock_session_tokens_with_refresh();

        // Enable cross-process lock.
        oauth.enable_cross_process_refresh_lock("lock".to_owned()).await?;

        // Restore the session.
        oauth
            .restore_session(
                mock_session(mock_prev_session_tokens_with_refresh()),
                RoomLoadSettings::default(),
            )
            .await?;

        // Immediately try to refresh the access token twice in parallel.
        for result in join_all([oauth.refresh_access_token(), oauth.refresh_access_token()]).await {
            result?;
        }

        {
            // The cross process lock has been correctly updated, and the next attempt to
            // take it won't result in a mismatch.
            let xp_manager =
                oauth.ctx().cross_process_token_refresh_manager.get().context("must have lock")?;
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
        let server = MatrixMockServer::new().await;

        let oauth_server = server.oauth();
        oauth_server.mock_server_metadata().ok().expect(1..).named("server_metadata").mount().await;
        oauth_server.mock_token().ok().expect(1).named("token").mount().await;

        let prev_tokens = mock_prev_session_tokens_with_refresh();
        let next_tokens = mock_session_tokens_with_refresh();

        // Create the first client.
        let tmp_dir = tempfile::tempdir()?;
        let client = server
            .client_builder()
            .on_builder(|builder| builder.sqlite_store(&tmp_dir, None))
            .unlogged()
            .build()
            .await;

        let oauth = client.oauth();
        oauth.enable_cross_process_refresh_lock("client1".to_owned()).await?;

        oauth
            .restore_session(mock_session(prev_tokens.clone()), RoomLoadSettings::default())
            .await?;

        // Create a second client, without restoring it, to test that a token update
        // before restoration doesn't cause new issues.
        let unrestored_client = server
            .client_builder()
            .on_builder(|builder| builder.sqlite_store(&tmp_dir, None))
            .unlogged()
            .build()
            .await;
        let unrestored_oauth = unrestored_client.oauth();
        unrestored_oauth.enable_cross_process_refresh_lock("unrestored_client".to_owned()).await?;

        {
            // Create a third client that will run a refresh while the others two are doing
            // nothing.
            let client3 = server
                .client_builder()
                .on_builder(|builder| builder.sqlite_store(&tmp_dir, None))
                .unlogged()
                .build()
                .await;

            let oauth3 = client3.oauth();
            oauth3.enable_cross_process_refresh_lock("client3".to_owned()).await?;
            oauth3
                .restore_session(mock_session(prev_tokens.clone()), RoomLoadSettings::default())
                .await?;

            // Run a refresh in the second client; this will invalidate the tokens from the
            // first token.
            oauth3.refresh_access_token().await?;

            assert_eq!(client3.session_tokens(), Some(next_tokens.clone()));

            // Reading from the cross-process lock for the second client only shows the new
            // tokens.
            let xp_manager =
                oauth3.ctx().cross_process_token_refresh_manager.get().context("must have lock")?;
            let guard = xp_manager.spin_lock().await?;
            let actual_hash = compute_session_hash(&next_tokens);
            assert_eq!(guard.db_hash.as_ref(), Some(&actual_hash));
            assert_eq!(guard.hash_guard.as_ref(), Some(&actual_hash));
            assert!(!guard.hash_mismatch);
        }

        {
            // Restoring the client that was not restored yet will work Just Fine.
            let oauth = unrestored_oauth;

            unrestored_client.set_session_callbacks(
                Box::new({
                    // This is only called because of extra checks in the code.
                    let tokens = next_tokens.clone();
                    move |_| Ok(tokens.clone())
                }),
                Box::new(|_| panic!("save_session_callback shouldn't be called here")),
            )?;

            oauth
                .restore_session(mock_session(prev_tokens.clone()), RoomLoadSettings::default())
                .await?;

            // And this client is now aware of the latest tokens.
            let xp_manager =
                oauth.ctx().cross_process_token_refresh_manager.get().context("must have lock")?;
            let guard = xp_manager.spin_lock().await?;
            let next_hash = compute_session_hash(&next_tokens);
            assert_eq!(guard.db_hash.as_ref(), Some(&next_hash));
            assert_eq!(guard.hash_guard.as_ref(), Some(&next_hash));
            assert!(!guard.hash_mismatch);

            drop(oauth);
            drop(unrestored_client);
        }

        {
            // The cross process lock has been correctly updated, and the next attempt to
            // take it will result in a mismatch.
            let xp_manager =
                oauth.ctx().cross_process_token_refresh_manager.get().context("must have lock")?;
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
                move |_| Ok(tokens.clone())
            }),
            Box::new(|_| panic!("save_session_callback shouldn't be called here")),
        )?;

        oauth.refresh_access_token().await?;

        {
            // The next attempt to take the lock isn't a mismatch.
            let xp_manager =
                oauth.ctx().cross_process_token_refresh_manager.get().context("must have lock")?;
            let guard = xp_manager.spin_lock().await?;
            let actual_hash = compute_session_hash(&next_tokens);
            assert_eq!(guard.db_hash.as_ref(), Some(&actual_hash));
            assert_eq!(guard.hash_guard.as_ref(), Some(&actual_hash));
            assert!(!guard.hash_mismatch);
        }

        Ok(())
    }

    #[async_test]
    async fn test_logout() -> anyhow::Result<()> {
        let server = MatrixMockServer::new().await;

        let oauth_server = server.oauth();
        oauth_server
            .mock_server_metadata()
            .ok_https()
            .expect(1..)
            .named("server_metadata")
            .mount()
            .await;
        oauth_server.mock_revocation().ok().expect(1).named("revocation").mount().await;

        let tmp_dir = tempfile::tempdir()?;
        let client = server
            .client_builder()
            .on_builder(|builder| builder.sqlite_store(&tmp_dir, None))
            .unlogged()
            .build()
            .await;
        let oauth = client.oauth().insecure_rewrite_https_to_http();

        // Enable cross-process lock.
        oauth.enable_cross_process_refresh_lock("lock".to_owned()).await?;

        // Restore the session.
        let tokens = mock_session_tokens_with_refresh();
        oauth.restore_session(mock_session(tokens.clone()), RoomLoadSettings::default()).await?;

        oauth.logout().await.unwrap();

        {
            // The cross process lock has been correctly updated, and all the hashes are
            // empty after a logout.
            let xp_manager =
                oauth.ctx().cross_process_token_refresh_manager.get().context("must have lock")?;
            let guard = xp_manager.spin_lock().await?;
            assert!(guard.db_hash.is_none());
            assert!(guard.hash_guard.is_none());
            assert!(!guard.hash_mismatch);
        }

        Ok(())
    }

    #[test]
    fn test_session_hash_to_hex() {
        let hash = SessionHash(vec![]);
        assert_eq!(hash.to_hex(), "");

        let hash = SessionHash(vec![0x13, 0x37, 0x42, 0xde, 0xad, 0xca, 0xfe]);
        assert_eq!(hash.to_hex(), "0x133742deadcafe");
    }
}
