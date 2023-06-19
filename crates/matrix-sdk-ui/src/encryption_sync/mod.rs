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

use std::time::Duration;

use async_stream::stream;
use futures_core::stream::Stream;
use futures_util::{pin_mut, StreamExt};
use matrix_sdk::{Client, SlidingSync};
use matrix_sdk_crypto::CryptoStoreError;
use ruma::{api::client::sync::sync_events::v4, assign};
use tracing::error;

#[derive(Clone, Copy)]
pub enum EncryptionSyncMode {
    /// Run the loop for a fixed amount of iterations.
    RunFixedIterations(u8),

    /// Never stop running the loop, except if asked to stop.
    NeverStop,
}

/// High-level helper for synchronizing encryption events using sliding sync.
///
/// See the module's documentation for more details.
pub struct EncryptionSync {
    client: Client,
    sliding_sync: SlidingSync,
    mode: EncryptionSyncMode,
    with_lock: bool,
}

impl EncryptionSync {
    /// Creates a new instance of a `EncryptionSync`.
    ///
    /// This will create and manage an instance of [`matrix_sdk::SlidingSync`].
    /// The `id` is used as the identifier of that instance, as such make
    /// sure to not reuse a name used by another Sliding Sync instance, at
    /// the risk of causing problems.
    pub async fn new(
        id: impl Into<String>,
        client: Client,
        mode: EncryptionSyncMode,
        with_lock: bool,
    ) -> Result<Self, Error> {
        let id = id.into();
        let mut builder = client
            .sliding_sync(id.clone())?
            .enable_caching()?
            .with_to_device_extension(
                assign!(v4::ToDeviceConfig::default(), { enabled: Some(true)}),
            )
            .with_e2ee_extension(assign!(v4::E2EEConfig::default(), { enabled: Some(true)}));

        if matches!(mode, EncryptionSyncMode::RunFixedIterations(..)) {
            builder = builder.with_timeouts(Duration::from_secs(4), Duration::from_secs(4));
        }

        let sliding_sync = builder.build().await?;

        if with_lock {
            // Gently try to set the cross-process lock on behalf of the user.
            match client.encryption().enable_cross_process_store_lock(id).await {
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

        Ok(Self { client, sliding_sync, mode, with_lock })
    }

    /// Start synchronization.
    ///
    /// This should be regularly polled.
    pub fn sync(&self) -> impl Stream<Item = Result<(), Error>> + '_ {
        stream!({
            let sync = self.sliding_sync.sync();

            pin_mut!(sync);

            let mut mode = self.mode;

            loop {
                let must_unlock = match &mut mode {
                    EncryptionSyncMode::RunFixedIterations(ref mut val) => {
                        if *val == 0 {
                            // The previous attempt was the last one, stop now.
                            break;
                        }
                        // Soon.
                        *val -= 1;

                        self.with_lock && self.client.encryption().try_lock_store_once().await?
                    }

                    EncryptionSyncMode::NeverStop => {
                        self.client.encryption().spin_lock_store(Some(60000)).await?;
                        true
                    }
                };

                match sync.next().await {
                    Some(Ok(update_summary)) => {
                        // This API is only concerned with the e2ee and to-device extensions.
                        // Warn if anything weird has been received from the proxy.
                        if !update_summary.lists.is_empty() {
                            error!(?update_summary.lists, "unexpected non-empty list of lists in encryption sync API");
                        }
                        if !update_summary.rooms.is_empty() {
                            error!(?update_summary.rooms, "unexpected non-empty list of rooms in encryption sync API");
                        }

                        if must_unlock {
                            self.client.encryption().unlock_store().await?;
                        }

                        // Cool cool, let's do it again.
                        yield Ok(());

                        continue;
                    }

                    Some(Err(err)) => {
                        if must_unlock {
                            self.client.encryption().unlock_store().await?;
                        }

                        yield Err(err.into());

                        break;
                    }

                    None => {
                        if must_unlock {
                            self.client.encryption().unlock_store().await?;
                        }

                        break;
                    }
                }
            }
        })
    }

    pub fn stop(&self) -> Result<(), Error> {
        // Stopping the sync loop will cause the next `next()` call to return `None`, so
        // this will also release the cross-process lock automatically.
        self.sliding_sync.stop_sync()?;

        Ok(())
    }
}

/// Errors for the [`EncryptionSync`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Something wrong happened in sliding sync: {0:#}")]
    SlidingSync(#[from] matrix_sdk::Error),

    #[error("The client was not authenticated")]
    AuthenticationRequired,

    #[error(transparent)]
    CryptoStore(#[from] CryptoStoreError),

    #[error("The crypto store state couldn't be reloaded")]
    ReloadCryptoStoreError,

    #[error(transparent)]
    ClientError(matrix_sdk::Error),
}
