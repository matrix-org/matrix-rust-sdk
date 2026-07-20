// Copyright 2026 The Matrix.org Foundation C.I.C.
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

//! High-level interface for [Dehydrated Devices] ([MSC3814]).
//!
//! A dehydrated device is a virtual device the homeserver keeps on the user's
//! behalf while no live device is online. Senders can encrypt to it using the
//! same Olm session establishment as for any other device. When a new device
//! comes online it rehydrates: it pulls the private parts of the virtual
//! device back down, decrypts them with a pickle key, drains the queued
//! to-device events, and imports the room keys they carry.
//!
//! # Lifecycle
//!
//! 1. `is_supported`: cheap probe of the homeserver.
//! 2. `create`: build a fresh dehydrated device and upload it. The pickle key
//!    is supplied by the caller; storage and rotation of the pickle key are an
//!    application concern.
//! 3. `rehydrate`: pull the existing dehydrated device, decrypt with the pickle
//!    key, absorb queued to-device events, and delete the device.
//! 4. `delete`: remove the current dehydrated device without rehydrating.
//!
//! # Example
//!
//! ```no_run
//! # use matrix_sdk::Client;
//! # use matrix_sdk_base::crypto::store::types::DehydratedDeviceKey;
//! # async fn example(client: Client) -> anyhow::Result<()> {
//! let dehydrated = client.encryption().dehydrated_devices();
//!
//! if !dehydrated.is_supported().await? {
//!     return Ok(());
//! }
//!
//! let pickle_key = DehydratedDeviceKey::new();
//!
//! dehydrated.rehydrate(&pickle_key).await?;
//! dehydrated.create(None, &pickle_key).await?;
//! # Ok(())
//! # }
//! ```
//!
//! [Dehydrated Devices]: https://spec.matrix.org/unstable/client-server-api/#dehydrated-devices
//! [MSC3814]: https://github.com/matrix-org/matrix-spec-proposals/pull/3814

use std::{future::IntoFuture, time::Duration};

use futures_core::Stream;
use matrix_sdk_base::crypto::{
    DecryptionSettings, OlmError, TrustRequirement,
    dehydrated_devices::{DehydrationError, RehydratedDevice},
    store::types::DehydratedDeviceKey,
    vodozemac::base64_decode,
};
use matrix_sdk_common::{
    boxed_into_future, locks::Mutex as StdMutex, sleep::sleep, task_monitor::BackgroundTaskHandle,
};
use ruma::{
    OwnedDeviceId,
    api::{
        client::dehydrated_device::{
            DehydratedDeviceData, delete_dehydrated_device, get_dehydrated_device, get_events,
        },
        error::ErrorKind,
    },
    events::secret::request::SecretName,
    serde::Raw,
};
use thiserror::Error;
use tokio::sync::broadcast;
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};
use tracing::{Instrument, Span, debug, info, instrument, trace, warn};
use zeroize::Zeroizing;

use crate::{
    Client, HttpError,
    client::WeakClient,
    encryption::{CryptoStoreError, secret_storage::SecretStore},
};

/// The default display name uploaded for a freshly created dehydrated device.
const DEFAULT_DEVICE_DISPLAY_NAME: &str = "Dehydrated device";

/// The name used to store the dehydrated-device pickle key in Secret Storage.
///
/// MSC3814 reserves `m.dehydrated_device` for the stable name; this is the
/// unstable equivalent the implementation will publish until the MSC
/// stabilizes.
const PICKLE_KEY_SECRET_NAME: &str = "org.matrix.msc3814";

/// How often the rotation task started by [`DehydratedDevices::start`]
/// re-creates the dehydrated device. Fixed at one week.
const DEHYDRATION_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Crypto-store key under which the ID of the most recently uploaded dehydrated
/// device is persisted, so the replay check in [`DehydratedDevices::rehydrate`]
/// survives a client restart.
const LAST_UPLOADED_DEVICE_ID_KEY: &str = "matrix-sdk-dehydrated-devices.last-uploaded-device-id";

/// Errors that can occur while managing dehydrated devices.
#[derive(Debug, Error)]
pub enum DehydratedDeviceError {
    /// The HTTP request to the homeserver failed.
    #[error(transparent)]
    Http(#[from] HttpError),

    /// The cryptographic operation on the dehydrated device failed.
    #[error(transparent)]
    Crypto(#[from] DehydrationError),

    /// Importing room keys from a rehydrated device's to-device events
    /// failed.
    #[error(transparent)]
    Olm(#[from] OlmError),

    /// The crypto store could not be accessed.
    #[error(transparent)]
    Store(#[from] CryptoStoreError),

    /// Reading or writing a secret to Secret Storage failed.
    #[error(transparent)]
    SecretStorage(#[from] crate::encryption::secret_storage::SecretStorageError),

    /// The pickle key stored in Secret Storage was not valid base64.
    #[error("the dehydrated-device pickle key in Secret Storage is not valid base64: {0}")]
    PickleKeyDecode(#[from] vodozemac::Base64DecodeError),

    /// The client is not logged in; the Olm machine is not available.
    #[error("the client is not logged in")]
    NotLoggedIn,
}

/// Return a [`SecretName`] for the dehydrated-device pickle key entry.
fn pickle_key_secret_name() -> SecretName {
    SecretName::from(PICKLE_KEY_SECRET_NAME)
}

/// Lifecycle events emitted by [`DehydratedDevices`].
///
/// Subscribe with [`DehydratedDevices::state_stream`] to observe creation,
/// rehydration progress, and rotation outcomes. [`Self::RehydrationCompleted`]
/// carries the final imported counts so a caller does not have to fold over the
/// [`Self::RehydrationProgress`] events, and [`Self::RotationError`] surfaces
/// background rotation failures the task would otherwise swallow.
#[derive(Clone, Debug)]
pub enum DehydratedDeviceEvent {
    /// A fresh dehydrated device was constructed in the local crypto
    /// store, before the upload PUT.
    Created {
        /// Device ID assigned to the new dehydrated device.
        device_id: OwnedDeviceId,
    },
    /// The dehydrated device announced by the preceding
    /// [`Self::Created`] event was accepted by the homeserver.
    Uploaded {
        /// Device ID of the dehydrated device now visible on the server.
        device_id: OwnedDeviceId,
    },
    /// The dehydrated device currently on the server was deleted.
    Deleted,
    /// A pickle key was cached in the local crypto store.
    KeyCached,
    /// Rehydration of a dehydrated device began.
    RehydrationStarted {
        /// Device ID of the dehydrated device being rehydrated.
        device_id: OwnedDeviceId,
    },
    /// A batch of to-device events has been imported during rehydration.
    RehydrationProgress {
        /// Cumulative number of room keys imported so far.
        room_keys_imported: usize,
        /// Cumulative number of to-device events processed so far.
        to_device_events: usize,
    },
    /// Rehydration finished successfully.
    RehydrationCompleted {
        /// Device ID of the rehydrated device.
        device_id: OwnedDeviceId,
        /// Total number of room keys imported.
        room_keys_imported: usize,
        /// Total number of to-device events processed.
        to_device_events: usize,
    },
    /// Rehydration failed before it could complete.
    RehydrationError {
        /// Human-readable description of the failure.
        error: String,
    },
    /// A scheduled rotation tick failed; the rotation task remains
    /// scheduled and will retry at the next tick.
    RotationError {
        /// Human-readable description of the failure.
        error: String,
    },
}

/// Process-wide state for the dehydrated-devices manager.
///
/// Held inside [`crate::encryption::EncryptionData`] so the event sender
/// and any in-flight rotation task survive across
/// `Client::encryption().dehydrated_devices()` calls.
pub(crate) struct DehydratedDevicesState {
    event_sender: broadcast::Sender<DehydratedDeviceEvent>,
    rotation_task: StdMutex<Option<BackgroundTaskHandle>>,
}

impl Default for DehydratedDevicesState {
    fn default() -> Self {
        let (event_sender, _) = broadcast::channel(32);
        Self { event_sender, rotation_task: StdMutex::new(None) }
    }
}

/// High-level handle returned by
/// [`Encryption::dehydrated_devices`](crate::encryption::Encryption::dehydrated_devices).
#[derive(Debug, Clone)]
pub struct DehydratedDevices {
    pub(super) client: Client,
}

/// The dehydrated device the server currently holds on the user's behalf.
struct DownloadedDevice {
    device_id: OwnedDeviceId,
    device_data: Raw<DehydratedDeviceData>,
}

impl DehydratedDevices {
    /// Subscribe to the stream of [`DehydratedDeviceEvent`]s.
    ///
    /// Each call returns a fresh stream. If a subscriber is slow enough to
    /// fall behind the channel's buffer, it receives a
    /// [`BroadcastStreamRecvError`] reporting the number of skipped events
    /// and the stream continues from the most recent event.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use futures_util::StreamExt;
    /// # async fn example(client: Client) -> anyhow::Result<()> {
    /// let dehydrated = client.encryption().dehydrated_devices();
    /// let mut stream = dehydrated.state_stream();
    /// while let Some(Ok(event)) = stream.next().await {
    ///     println!("dehydrated devices: {event:?}");
    /// }
    /// # Ok(()) }
    /// ```
    pub fn state_stream(
        &self,
    ) -> impl Stream<Item = Result<DehydratedDeviceEvent, BroadcastStreamRecvError>> + use<> {
        BroadcastStream::new(self.state().event_sender.subscribe())
    }

    fn state(&self) -> &DehydratedDevicesState {
        &self.client.inner.e2ee.dehydrated_devices_state
    }

    fn emit(&self, event: DehydratedDeviceEvent) {
        // A send failure means there are no subscribers; that is fine.
        let _ = self.state().event_sender.send(event);
    }

    /// Return whether the homeserver advertises dehydrated-device support.
    ///
    /// Probes by issuing `GET /dehydrated_device` and inspecting the errcode
    /// of the response:
    ///
    /// - `M_UNRECOGNIZED` means the server does not understand the endpoint.
    /// - `M_NOT_FOUND` or a successful response means the server understands
    ///   the endpoint (whether or not the user currently has a dehydrated
    ///   device on file).
    ///
    /// Any other transport or API failure is propagated.
    #[instrument(skip_all)]
    pub async fn is_supported(&self) -> Result<bool, DehydratedDeviceError> {
        let request = get_dehydrated_device::unstable::Request::new();
        match self.client.send(request).await {
            Ok(_) => Ok(true),
            Err(e) => match e.client_api_error_kind() {
                Some(ErrorKind::Unrecognized) => Ok(false),
                Some(ErrorKind::NotFound) => Ok(true),
                _ => Err(e.into()),
            },
        }
    }

    /// Create a fresh dehydrated device and upload it to the homeserver.
    ///
    /// The pickle key is used by [vodozemac] to encrypt the private parts of
    /// the device. The application is responsible for safely storing the
    /// pickle key (typically in Secret Storage so future sessions can
    /// rehydrate the device).
    ///
    /// # Arguments
    ///
    /// * `display_name` - Optional human-readable name uploaded as the
    ///   dehydrated device's `initial_device_display_name`. Defaults to
    ///   `"Dehydrated device"`.
    /// * `pickle_key` - 32-byte key used to encrypt the dehydrated device.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk_base::crypto::store::types::DehydratedDeviceKey;
    /// # async fn example(client: Client) -> anyhow::Result<()> {
    /// let pickle_key = DehydratedDeviceKey::new();
    /// let device_id = client
    ///     .encryption()
    ///     .dehydrated_devices()
    ///     .create(Some("Offline catcher"), &pickle_key)
    ///     .await?;
    /// println!("Uploaded dehydrated device {device_id}");
    /// # Ok(()) }
    /// ```
    ///
    /// [vodozemac]: https://docs.rs/vodozemac/
    #[instrument(skip_all)]
    pub async fn create(
        &self,
        display_name: Option<&str>,
        pickle_key: &DehydratedDeviceKey,
    ) -> Result<OwnedDeviceId, DehydratedDeviceError> {
        let olm = self.client.olm_machine().await;
        let machine = olm.as_ref().ok_or(DehydratedDeviceError::NotLoggedIn)?;

        debug!("Creating a new dehydrated device in the crypto store");
        let dehydrated_device = machine.dehydrated_devices().create().await?;

        let display_name = display_name.unwrap_or(DEFAULT_DEVICE_DISPLAY_NAME);
        let request =
            dehydrated_device.keys_for_upload(display_name.to_owned(), pickle_key).await?;
        let device_id = request.device_id.clone();
        self.emit(DehydratedDeviceEvent::Created { device_id: device_id.clone() });

        debug!(?device_id, "Uploading dehydrated device to the homeserver");
        self.client.send(request).await?;
        info!(?device_id, "Successfully uploaded dehydrated device");

        machine.store().set_value(LAST_UPLOADED_DEVICE_ID_KEY, &device_id).await?;
        self.emit(DehydratedDeviceEvent::Uploaded { device_id: device_id.clone() });
        Ok(device_id)
    }

    /// Rehydrate the dehydrated device currently on the server, if any.
    ///
    /// Downloads the dehydrated device, decrypts it with `pickle_key`,
    /// drains all queued to-device events to import their room keys, and
    /// finally deletes the device from the server.
    ///
    /// Returns `Ok(false)` if the server reports no dehydrated device
    /// (`M_NOT_FOUND`) or does not implement the endpoint
    /// (`M_UNRECOGNIZED`). Returns `Ok(true)` once the rehydration cycle
    /// has completed end to end.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk_base::crypto::store::types::DehydratedDeviceKey;
    /// # async fn example(client: Client, pickle_key: DehydratedDeviceKey)
    /// # -> anyhow::Result<()> {
    /// let rehydrated =
    ///     client.encryption().dehydrated_devices().rehydrate(&pickle_key).await?;
    /// if rehydrated {
    ///     println!("Caught up on offline room keys");
    /// }
    /// # Ok(()) }
    /// ```
    #[instrument(skip_all)]
    pub async fn rehydrate(
        &self,
        pickle_key: &DehydratedDeviceKey,
    ) -> Result<bool, DehydratedDeviceError> {
        let Some(downloaded) = self.download_device().await? else { return Ok(false) };
        info!(device_id = ?downloaded.device_id, "Dehydrated device found");

        // The server should serve back whichever id we last uploaded. A
        // mismatch can indicate a stale or replayed payload; warn so the
        // application can decide to drop or quarantine the imported keys. The
        // last-uploaded id is persisted in the crypto store, so this check
        // holds across restarts.
        if let Some(expected) = self.last_uploaded_device_id().await?
            && expected != downloaded.device_id
        {
            warn!(
                ?expected,
                got = ?downloaded.device_id,
                "Server returned a different dehydrated-device id than the one we last uploaded; continuing but the payload may be stale"
            );
        }

        self.emit(DehydratedDeviceEvent::RehydrationStarted {
            device_id: downloaded.device_id.clone(),
        });

        let rehydrated = self.rehydrate_device(&downloaded, pickle_key).await?;
        let (room_keys_imported, to_device_events) =
            self.absorb_events(&downloaded.device_id, &rehydrated).await?;

        self.emit(DehydratedDeviceEvent::RehydrationCompleted {
            device_id: downloaded.device_id.clone(),
            room_keys_imported,
            to_device_events,
        });

        // Key import already succeeded; if the post-drain delete fails, log
        // it but do not let the failure masquerade as a rehydration error.
        // The next create() call will replace the device anyway.
        if let Err(e) = self.delete().await {
            warn!(device_id = ?downloaded.device_id, error = %e, "Post-rehydration delete failed; the next rotation will replace the device");
        }

        Ok(true)
    }

    /// Cache the pickle key in the local crypto store.
    ///
    /// Subsequent rehydration attempts can then resolve the key from the
    /// cache without an account-data round-trip.
    #[instrument(skip_all)]
    pub(crate) async fn cache_key(
        &self,
        pickle_key: &DehydratedDeviceKey,
    ) -> Result<(), DehydratedDeviceError> {
        let olm = self.client.olm_machine().await;
        let machine = olm.as_ref().ok_or(DehydratedDeviceError::NotLoggedIn)?;

        machine.dehydrated_devices().save_dehydrated_device_pickle_key(pickle_key).await?;
        self.emit(DehydratedDeviceEvent::KeyCached);
        Ok(())
    }

    /// Return the pickle key currently cached in the local crypto store.
    ///
    /// `Ok(None)` if no key has been cached. The returned key matches the
    /// last value persisted via [`cache_key`](Self::cache_key) or
    /// [`reset_key`](Self::reset_key); it is not fetched from Secret Storage.
    #[instrument(skip_all)]
    pub(crate) async fn cached_key(
        &self,
    ) -> Result<Option<DehydratedDeviceKey>, DehydratedDeviceError> {
        let olm = self.client.olm_machine().await;
        let machine = olm.as_ref().ok_or(DehydratedDeviceError::NotLoggedIn)?;

        Ok(machine.dehydrated_devices().get_dehydrated_device_pickle_key().await?)
    }

    /// Return the ID of the dehydrated device this client most recently
    /// uploaded, as persisted in the crypto store, or `Ok(None)` if none has
    /// been uploaded yet.
    async fn last_uploaded_device_id(
        &self,
    ) -> Result<Option<OwnedDeviceId>, DehydratedDeviceError> {
        let olm = self.client.olm_machine().await;
        let machine = olm.as_ref().ok_or(DehydratedDeviceError::NotLoggedIn)?;

        Ok(machine.store().get_value(LAST_UPLOADED_DEVICE_ID_KEY).await?)
    }

    /// Return whether the pickle key is stored in the given Secret Storage.
    ///
    /// The key is looked up by the account-data event type
    /// `org.matrix.msc3814` (the unstable name reserved by MSC3814).
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, encryption::secret_storage::SecretStore};
    /// # async fn example(client: Client, store: SecretStore)
    /// # -> anyhow::Result<()> {
    /// let stored =
    ///     client.encryption().dehydrated_devices().is_key_stored(&store).await?;
    /// # Ok(()) }
    /// ```
    pub async fn is_key_stored(
        &self,
        secret_store: &SecretStore,
    ) -> Result<bool, DehydratedDeviceError> {
        Ok(secret_store.get_secret(pickle_key_secret_name()).await?.is_some())
    }

    /// Generate a new random pickle key, persist it in Secret Storage, and
    /// cache it in the local crypto store.
    ///
    /// The previous key (if any) is overwritten in both places. Any
    /// dehydrated device that was encrypted with the previous key becomes
    /// unrehydratable until rotated.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, encryption::secret_storage::SecretStore};
    /// # async fn example(client: Client, store: SecretStore)
    /// # -> anyhow::Result<()> {
    /// let fresh_key =
    ///     client.encryption().dehydrated_devices().reset_key(&store).await?;
    /// # let _ = fresh_key;
    /// # Ok(()) }
    /// ```
    #[instrument(skip_all)]
    pub async fn reset_key(
        &self,
        secret_store: &SecretStore,
    ) -> Result<DehydratedDeviceKey, DehydratedDeviceError> {
        let key = DehydratedDeviceKey::new();
        secret_store.put_secret(pickle_key_secret_name(), &key.to_base64()).await?;
        self.cache_key(&key).await?;
        Ok(key)
    }

    /// Resolve the pickle key.
    ///
    /// Looks for the key in this order:
    ///
    /// 1. The local crypto-store cache.
    /// 2. The provided Secret Storage account-data entry. A successfully
    ///    fetched key is written back to the cache.
    /// 3. If `create_if_missing`, a fresh random key is generated, stored, and
    ///    cached.
    ///
    /// Returns `Ok(None)` only when the key is absent from both sources and
    /// `create_if_missing` is `false`.
    #[instrument(skip_all)]
    pub(crate) async fn load_key(
        &self,
        secret_store: &SecretStore,
        create_if_missing: bool,
    ) -> Result<Option<DehydratedDeviceKey>, DehydratedDeviceError> {
        if let Some(cached) = self.cached_key().await? {
            return Ok(Some(cached));
        }

        let Some(base64) = secret_store.get_secret(pickle_key_secret_name()).await? else {
            return if create_if_missing {
                Ok(Some(self.reset_key(secret_store).await?))
            } else {
                Ok(None)
            };
        };

        let bytes = Zeroizing::new(base64_decode(&base64)?);
        let key = DehydratedDeviceKey::from_slice(&bytes)?;
        self.cache_key(&key).await?;
        Ok(Some(key))
    }

    /// Start using dehydrated devices for this client.
    ///
    /// Returns a [`StartDehydration`] future. Await it directly for the default
    /// behavior, or configure it first with
    /// [`StartDehydration::create_new_key`],
    /// [`StartDehydration::skip_rehydration`], and
    /// [`StartDehydration::only_if_key_cached`].
    ///
    /// The caller is expected to have unlocked Secret Storage and bootstrapped
    /// cross-signing before this call; otherwise the underlying account-data
    /// reads and writes will fail mid-flight.
    ///
    /// Awaiting the future performs the following steps:
    ///
    /// 1. If [`StartDehydration::only_if_key_cached`] was set, return early
    ///    when no pickle key is cached locally.
    /// 2. Stop any previously scheduled rotation.
    /// 3. Unless [`StartDehydration::skip_rehydration`] was set, attempt to
    ///    rehydrate the existing dehydrated device. Failures are logged and
    ///    emitted as [`DehydratedDeviceEvent::RehydrationError`] but do not
    ///    abort the start.
    /// 4. If [`StartDehydration::create_new_key`] was set and the rehydration
    ///    step succeeded (or was skipped), replace the pickle key in Secret
    ///    Storage with a fresh random one. A failed rehydration suppresses the
    ///    reset so the stored key can still recover the existing dehydrated
    ///    device on another client.
    /// 5. Create a new dehydrated device now and schedule rotation once a week.
    ///
    /// The rotation task resolves the pickle key from the local crypto store on
    /// each tick. The local cache is the only key source available to the task
    /// because reopening Secret Storage requires the recovery key, which is not
    /// retained. Per-tick failures emit
    /// [`DehydratedDeviceEvent::RotationError`] without aborting the schedule.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, encryption::secret_storage::SecretStore};
    /// # async fn example(client: Client, store: SecretStore)
    /// # -> anyhow::Result<()> {
    /// client.encryption().dehydrated_devices().start(&store).await?;
    /// # Ok(()) }
    /// ```
    pub fn start<'a>(&'a self, secret_store: &'a SecretStore) -> StartDehydration<'a> {
        StartDehydration::new(self, secret_store)
    }

    /// Stop the scheduled dehydrated-device rotation, if any.
    ///
    /// Has no effect when no rotation is scheduled. Existing dehydrated
    /// devices on the server are left in place; pair with
    /// [`Self::delete`] to clean those up.
    pub fn stop(&self) {
        self.state().rotation_task.lock().take();
    }

    /// Create-and-upload the first dehydrated device now, then spawn the
    /// rotation loop.
    async fn schedule_dehydration(
        &self,
        secret_store: &SecretStore,
    ) -> Result<(), DehydratedDeviceError> {
        let key = self
            .load_key(secret_store, true)
            .await?
            .expect("load_key(create_if_missing=true) always yields a key");
        self.create(None, &key).await?;

        let weak_client = WeakClient::from_client(&self.client);
        let handle = self
            .client
            .task_monitor()
            .spawn_infinite_task("dehydrated_devices::rotation", async move {
                loop {
                    sleep(DEHYDRATION_INTERVAL).await;

                    let Some(client) = weak_client.get() else {
                        // The client is gone; this task is aborted when its
                        // handle drops, so just wait for that to happen.
                        continue;
                    };

                    client.encryption().dehydrated_devices().rotate_tick().await;
                }
            })
            .abort_on_drop();

        *self.state().rotation_task.lock() = Some(handle);
        Ok(())
    }

    /// One iteration of the rotation timer: resolve the cached pickle key and
    /// upload a fresh dehydrated device, emitting a
    /// [`DehydratedDeviceEvent::RotationError`] on failure.
    async fn rotate_tick(&self) {
        let key = match self.cached_key().await {
            Ok(Some(key)) => key,
            Ok(None) => {
                let msg = "no cached pickle key for dehydrated-device rotation".to_owned();
                warn!("{msg}; skipping this rotation");
                self.emit(DehydratedDeviceEvent::RotationError { error: msg });
                return;
            }
            Err(e) => {
                let msg = e.to_string();
                warn!(error = %e, "Failed to load cached pickle key for rotation");
                self.emit(DehydratedDeviceEvent::RotationError { error: msg });
                return;
            }
        };

        if let Err(e) = self.create(None, &key).await {
            let msg = e.to_string();
            warn!(error = msg, "Failed to rotate dehydrated device");
            self.emit(DehydratedDeviceEvent::RotationError { error: msg });
        }
    }

    /// Delete the current dehydrated device, if one exists.
    ///
    /// Also stops any scheduled rotation, so the next tick will not
    /// immediately recreate the device the caller just asked to remove.
    ///
    /// Returns `Ok(())` silently if no dehydrated device is on the server or
    /// the server does not implement the endpoint.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # async fn example(client: Client) -> anyhow::Result<()> {
    /// client.encryption().dehydrated_devices().delete().await?;
    /// # Ok(()) }
    /// ```
    #[instrument(skip_all)]
    pub async fn delete(&self) -> Result<(), DehydratedDeviceError> {
        self.stop();
        let request = delete_dehydrated_device::unstable::Request::new();
        match self.client.send(request).await {
            Ok(_) => {
                self.emit(DehydratedDeviceEvent::Deleted);
                Ok(())
            }
            Err(e) => match e.client_api_error_kind() {
                Some(ErrorKind::Unrecognized) | Some(ErrorKind::NotFound) => Ok(()),
                _ => Err(e.into()),
            },
        }
    }

    /// Fetch the dehydrated device payload from the server.
    ///
    /// Returns `Ok(None)` if the server reports `M_NOT_FOUND` or
    /// `M_UNRECOGNIZED`.
    async fn download_device(&self) -> Result<Option<DownloadedDevice>, DehydratedDeviceError> {
        let request = get_dehydrated_device::unstable::Request::new();
        match self.client.send(request).await {
            Ok(response) => Ok(Some(DownloadedDevice {
                device_id: response.device_id,
                device_data: response.device_data,
            })),
            Err(e) => match e.client_api_error_kind() {
                Some(ErrorKind::NotFound) | Some(ErrorKind::Unrecognized) => Ok(None),
                _ => Err(e.into()),
            },
        }
    }

    /// Decrypt the downloaded device and stand up a [`RehydratedDevice`].
    async fn rehydrate_device(
        &self,
        downloaded: &DownloadedDevice,
        pickle_key: &DehydratedDeviceKey,
    ) -> Result<RehydratedDevice, DehydratedDeviceError> {
        let olm = self.client.olm_machine().await;
        let machine = olm.as_ref().ok_or(DehydratedDeviceError::NotLoggedIn)?;

        Ok(machine
            .dehydrated_devices()
            .rehydrate(pickle_key, &downloaded.device_id, downloaded.device_data.clone())
            .await?)
    }

    /// Drain every queued to-device event from the dehydrated device's
    /// server-side buffer, feeding each batch through the rehydrated
    /// machine so the room keys are imported. Returns
    /// `(room_keys_imported, to_device_events_processed)`.
    async fn absorb_events(
        &self,
        device_id: &OwnedDeviceId,
        rehydrated: &RehydratedDevice,
    ) -> Result<(usize, usize), DehydratedDeviceError> {
        let settings =
            DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

        let mut next_batch: Option<String> = None;
        let mut to_device_count: usize = 0;
        let mut room_key_count: usize = 0;

        loop {
            let mut request = get_events::unstable::Request::new(device_id.clone());
            request.next_batch.clone_from(&next_batch);

            let response = self.client.send(request).await?;
            if response.events.is_empty() {
                break;
            }

            to_device_count += response.events.len();
            let imported = rehydrated.receive_events(response.events, &settings).await?;
            room_key_count += imported.len();
            trace!(to_device_count, room_key_count, "Absorbed a batch of to-device events");
            self.emit(DehydratedDeviceEvent::RehydrationProgress {
                room_keys_imported: room_key_count,
                to_device_events: to_device_count,
            });

            // Defensive guard against a server that keeps returning the
            // same cursor or stops returning one altogether. matrix-js-sdk
            // does not have this; added here as a safety net.
            match response.next_batch {
                None => break,
                Some(ref token) if Some(token) == next_batch.as_ref() => {
                    warn!(
                        ?next_batch,
                        "Server returned the same next_batch twice; aborting to avoid an infinite loop"
                    );
                    break;
                }
                Some(token) => next_batch = Some(token),
            }
        }

        info!(to_device_count, room_key_count, "Drained dehydrated device to-device queue");
        Ok((room_key_count, to_device_count))
    }
}

/// Named future returned by [`DehydratedDevices::start`].
///
/// Await it to start dehydrated devices with the default behavior, or configure
/// it first with [`Self::create_new_key`], [`Self::skip_rehydration`], and
/// [`Self::only_if_key_cached`].
#[derive(Debug)]
pub struct StartDehydration<'a> {
    devices: &'a DehydratedDevices,
    secret_store: &'a SecretStore,
    create_new_key: bool,
    rehydrate: bool,
    only_if_key_cached: bool,
    tracing_span: Span,
}

impl<'a> StartDehydration<'a> {
    fn new(devices: &'a DehydratedDevices, secret_store: &'a SecretStore) -> Self {
        Self {
            devices,
            secret_store,
            create_new_key: false,
            rehydrate: true,
            only_if_key_cached: false,
            tracing_span: Span::current(),
        }
    }

    /// Generate a fresh random pickle key on start, replacing any existing
    /// entry in Secret Storage and the local cache.
    ///
    /// The reset is suppressed if a rehydration attempt in the same start
    /// fails, so the stored key can still recover the existing dehydrated
    /// device on another client.
    pub fn create_new_key(mut self) -> Self {
        self.create_new_key = true;
        self
    }

    /// Skip the attempt to rehydrate the existing dehydrated device before
    /// creating the next one.
    ///
    /// By default the existing device is rehydrated first.
    pub fn skip_rehydration(mut self) -> Self {
        self.rehydrate = false;
        self
    }

    /// Do nothing unless a pickle key is already cached locally.
    ///
    /// Useful for an opportunistic start on a freshly opened client without
    /// forcing a Secret Storage unlock.
    pub fn only_if_key_cached(mut self) -> Self {
        self.only_if_key_cached = true;
        self
    }
}

impl<'a> IntoFuture for StartDehydration<'a> {
    type Output = Result<(), DehydratedDeviceError>;
    boxed_into_future!(extra_bounds: 'a);

    fn into_future(self) -> Self::IntoFuture {
        let Self {
            devices,
            secret_store,
            create_new_key,
            rehydrate,
            only_if_key_cached,
            tracing_span,
        } = self;

        let future = async move {
            if only_if_key_cached && devices.cached_key().await?.is_none() {
                return Ok(());
            }

            devices.stop();

            let mut rehydrate_failed = false;
            if rehydrate
                && let Some(key) = devices.load_key(secret_store, false).await?
                && let Err(e) = devices.rehydrate(&key).await
            {
                let msg = e.to_string();
                warn!(error = %e, "Rehydration failed during start; continuing");
                devices.emit(DehydratedDeviceEvent::RehydrationError { error: msg });
                rehydrate_failed = true;
            }

            // Refuse to clobber Secret Storage after a failed rehydration: the
            // stored pickle key is the only one that can decrypt the existing
            // dehydrated device, and overwriting it would discard that recovery
            // chance for every other client signed in to this account.
            if create_new_key {
                if rehydrate_failed {
                    warn!(
                        "Skipping pickle-key reset after failed rehydration to preserve the chance of recovering the existing dehydrated device on another client"
                    );
                } else {
                    devices.reset_key(secret_store).await?;
                }
            }

            devices.schedule_dehydration(secret_store).await
        };

        Box::pin(future.instrument(tracing_span))
    }
}
