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
// See the License for that specific language governing permissions and
// limitations under the License.

//! Encryption Sync API.
//!
//! The encryption sync API is a high-level helper that is designed to take care
//! of handling the synchronization of encryption and to-device events (required
//! for encryption), be they received within the app or within a dedicated
//! extension process (e.g. the [NSE] process on iOS devices).
//!
//! Under the hood, this uses a sliding sync instance configured with no lists,
//! but that enables the e2ee and to-device extensions, so that it can both
//! handle encryption and manage encryption keys; that's sufficient to decrypt
//! messages received in the notification processes.
//!
//! [NSE]: https://developer.apple.com/documentation/usernotifications/unnotificationserviceextension

use std::{pin::Pin, time::Duration};

use async_stream::stream;
use futures_core::stream::Stream;
use futures_util::{pin_mut, StreamExt};
use matrix_sdk::{Client, SlidingSync, LEASE_DURATION_MS};
use ruma::{api::client::sync::sync_events::v4, assign};
use tokio::sync::OwnedMutexGuard;
use tracing::{debug, instrument, trace, Span};

/// Unit type representing a permit to *use* an [`EncryptionSyncService`].
///
/// This must be created once in the whole application's lifetime, wrapped in a
/// mutex. Using an `EncryptionSyncService` must then lock that mutex in an
/// owned way, so that there's at most a single `EncryptionSyncService` running
/// at any time in the entire app.
pub struct EncryptionSyncPermit(());

impl EncryptionSyncPermit {
    pub(crate) fn new() -> Self {
        Self(())
    }
}

impl EncryptionSyncPermit {
    /// Test-only.
    #[doc(hidden)]
    pub fn new_for_testing() -> Self {
        Self::new()
    }
}

/// Should the `EncryptionSyncService` make use of locking?
pub enum WithLocking {
    Yes,
    No,
}

impl From<bool> for WithLocking {
    fn from(value: bool) -> Self {
        if value {
            Self::Yes
        } else {
            Self::No
        }
    }
}

/// High-level helper for synchronizing encryption events using sliding sync.
///
/// See the module's documentation for more details.
pub struct EncryptionSyncService {
    client: Client,
    sliding_sync: SlidingSync,
    with_locking: bool,
}

impl EncryptionSyncService {
    /// Creates a new instance of a `EncryptionSyncService`.
    ///
    /// This will create and manage an instance of [`matrix_sdk::SlidingSync`].
    /// The `process_id` is used as the identifier of that instance, as such
    /// make sure to not reuse a name used by another process, at the risk
    /// of causing problems.
    pub async fn new(
        process_id: String,
        client: Client,
        poll_and_network_timeouts: Option<(Duration, Duration)>,
        with_locking: WithLocking,
    ) -> Result<Self, Error> {
        // Make sure to use the same `conn_id` and caching store identifier, whichever
        // process is running this sliding sync. There must be at most one
        // sliding sync instance that enables the e2ee and to-device extensions.
        let mut builder = client
            .sliding_sync("encryption")
            .map_err(Error::SlidingSync)?
            //.share_pos() // TODO(bnjbvr) This is racy, needs cross-process lock :')
            .with_to_device_extension(
                assign!(v4::ToDeviceConfig::default(), { enabled: Some(true)}),
            )
            .with_e2ee_extension(assign!(v4::E2EEConfig::default(), { enabled: Some(true)}));

        if let Some((poll_timeout, network_timeout)) = poll_and_network_timeouts {
            builder = builder.poll_timeout(poll_timeout).network_timeout(network_timeout);
        }

        let sliding_sync = builder.build().await.map_err(Error::SlidingSync)?;

        let with_locking = matches!(with_locking, WithLocking::Yes);

        if with_locking {
            // Gently try to enable the cross-process lock on behalf of the user.
            match client.encryption().enable_cross_process_store_lock(process_id).await {
                Ok(()) | Err(matrix_sdk::Error::BadCryptoStoreState) => {
                    // Ignore; we've already set the crypto store lock to
                    // something, and that's sufficient as
                    // long as it uniquely identifies the process.
                }
                Err(err) => {
                    // Any other error is fatal
                    return Err(Error::ClientError(err));
                }
            };
        }

        Ok(Self { client, sliding_sync, with_locking })
    }

    /// Runs an `EncryptionSyncService` loop for a fixed number of iterations.
    ///
    /// This runs for the given number of iterations, or less than that, if it
    /// stops earlier or could not acquire a cross-process lock (if configured
    /// with it).
    ///
    /// Note: the [`EncryptionSyncPermit`] parameter ensures that there's at
    /// most one encryption sync running at any time. See its documentation
    /// for more details.
    #[instrument(skip_all, fields(store_generation))]
    pub async fn run_fixed_iterations(
        self,
        num_iterations: u8,
        _permit: OwnedMutexGuard<EncryptionSyncPermit>,
    ) -> Result<(), Error> {
        let sync = self.sliding_sync.sync();

        pin_mut!(sync);

        let lock_guard = if self.with_locking {
            let mut lock_guard =
                self.client.encryption().try_lock_store_once().await.map_err(Error::LockError)?;

            // Try to take the lock at the beginning; if it's busy, that means that another
            // process already holds onto it, and as such we won't try to run the
            // encryption sync loop at all (because we expect the other process to
            // do so).

            if lock_guard.is_none() {
                // If we can't acquire the cross-process lock on the first attempt,
                // that means the main process is running, or its lease hasn't expired
                // yet. In case it's the latter, wait a bit and retry.
                tracing::debug!(
                    "Lock was already taken, and we're not the main loop; retrying in {}ms...",
                    LEASE_DURATION_MS
                );

                tokio::time::sleep(Duration::from_millis(LEASE_DURATION_MS.into())).await;

                lock_guard = self
                    .client
                    .encryption()
                    .try_lock_store_once()
                    .await
                    .map_err(Error::LockError)?;

                if lock_guard.is_none() {
                    tracing::debug!(
                        "Second attempt at locking outside the main app failed, aborting."
                    );
                    return Ok(());
                }
            }

            lock_guard
        } else {
            None
        };

        Span::current().record("store_generation", lock_guard.map(|guard| guard.generation()));

        for _ in 0..num_iterations {
            match sync.next().await {
                Some(Ok(update_summary)) => {
                    // This API is only concerned with the e2ee and to-device extensions.
                    // Warn if anything weird has been received from the proxy.
                    if !update_summary.lists.is_empty() {
                        debug!(?update_summary.lists, "unexpected non-empty list of lists in encryption sync API");
                    }
                    if !update_summary.rooms.is_empty() {
                        debug!(?update_summary.rooms, "unexpected non-empty list of rooms in encryption sync API");
                    }

                    // Cool cool, let's do it again.
                    trace!("Encryption sync received an update!");
                }

                Some(Err(err)) => {
                    trace!("Encryption sync stopped because of an error: {err:#}");
                    return Err(Error::SlidingSync(err));
                }

                None => {
                    trace!("Encryption sync properly terminated.");
                    break;
                }
            }
        }

        Ok(())
    }

    /// Start synchronization.
    ///
    /// This should be regularly polled.
    ///
    /// Note: the [`EncryptionSyncPermit`] parameter ensures that there's at
    /// most one encryption sync running at any time. See its documentation
    /// for more details.
    #[doc(hidden)] // Only public for testing purposes.
    pub fn sync(
        &self,
        _permit: OwnedMutexGuard<EncryptionSyncPermit>,
    ) -> impl Stream<Item = Result<(), Error>> + '_ {
        stream!({
            let sync = self.sliding_sync.sync();

            pin_mut!(sync);

            loop {
                match self.next_sync_with_lock(&mut sync).await? {
                    Some(Ok(update_summary)) => {
                        // This API is only concerned with the e2ee and to-device extensions.
                        // Warn if anything weird has been received from the proxy.
                        if !update_summary.lists.is_empty() {
                            debug!(?update_summary.lists, "unexpected non-empty list of lists in encryption sync API");
                        }
                        if !update_summary.rooms.is_empty() {
                            debug!(?update_summary.rooms, "unexpected non-empty list of rooms in encryption sync API");
                        }

                        // Cool cool, let's do it again.
                        trace!("Encryption sync received an update!");
                        yield Ok(());
                        continue;
                    }

                    Some(Err(err)) => {
                        trace!("Encryption sync stopped because of an error: {err:#}");
                        yield Err(Error::SlidingSync(err));
                        break;
                    }

                    None => {
                        trace!("Encryption sync properly terminated.");
                        break;
                    }
                }
            }
        })
    }

    /// Helper function for `sync`. Take the cross-process store lock, and call
    /// `sync.next()`
    #[instrument(skip_all, fields(store_generation))]
    async fn next_sync_with_lock<Item>(
        &self,
        sync: &mut Pin<&mut impl Stream<Item = Item>>,
    ) -> Result<Option<Item>, Error> {
        let guard = if self.with_locking {
            self.client.encryption().spin_lock_store(Some(60000)).await.map_err(Error::LockError)?
        } else {
            None
        };

        Span::current().record("store_generation", guard.map(|guard| guard.generation()));

        Ok(sync.next().await)
    }

    /// Requests that the underlying sliding sync be stopped.
    ///
    /// This will unlock the cross-process lock, if taken.
    pub(crate) fn stop_sync(&self) -> Result<(), Error> {
        // Stopping the sync loop will cause the next `next()` call to return `None`, so
        // this will also release the cross-process lock automatically.
        self.sliding_sync.stop_sync().map_err(Error::SlidingSync)?;

        Ok(())
    }

    pub(crate) async fn expire_sync_session(&self) {
        self.sliding_sync.expire_session().await;
    }
}

/// Errors for the [`EncryptionSyncService`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Something wrong happened in sliding sync: {0:#}")]
    SlidingSync(matrix_sdk::Error),

    #[error("Locking failed: {0:#}")]
    LockError(matrix_sdk::Error),

    #[error(transparent)]
    ClientError(matrix_sdk::Error),
}
