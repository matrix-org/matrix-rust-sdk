use std::{future, ops::Deref, sync::Arc};

use futures_core::Stream;
use futures_util::StreamExt;
use matrix_sdk_common::cross_process_lock::CrossProcessLock;
use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, UserId};
use tokio::sync::{Mutex, broadcast};
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};
use tracing::{debug, trace, warn};

use super::{
    DeviceChanges, IdentityChanges, LockableCryptoStore, caches::SessionStore,
    types::RoomKeyBundleInfo,
};
use crate::{
    CryptoStoreError, GossippedSecret, OwnUserIdentityData, Session, UserIdentityData,
    olm::InboundGroupSession,
    store,
    store::{Changes, DynCryptoStore, IntoCryptoStore, RoomKeyInfo, RoomKeyWithheldInfo},
};

/// A wrapper for crypto store implementations that adds update notifiers.
///
/// This is shared between [`StoreInner`] and
/// [`crate::verification::VerificationStore`].
#[derive(Debug)]
pub(crate) struct CryptoStoreWrapper {
    user_id: OwnedUserId,
    device_id: OwnedDeviceId,

    store: Arc<DynCryptoStore>,

    /// A cache for the Olm Sessions.
    sessions: SessionStore,

    /// The sender side of a broadcast stream that is notified whenever we get
    /// an update to an inbound group session.
    room_keys_received_sender: broadcast::Sender<Vec<RoomKeyInfo>>,

    /// The sender side of a broadcast stream that is notified whenever we
    /// receive an `m.room_key.withheld` message.
    room_keys_withheld_received_sender: broadcast::Sender<Vec<RoomKeyWithheldInfo>>,

    /// The sender side of a broadcast channel which sends out secrets we
    /// received as a `m.secret.send` event.
    secrets_broadcaster: broadcast::Sender<GossippedSecret>,

    /// The sender side of a broadcast channel which sends out devices and user
    /// identities which got updated or newly created.
    identities_broadcaster:
        broadcast::Sender<(Option<OwnUserIdentityData>, IdentityChanges, DeviceChanges)>,

    /// The sender side of a broadcast channel which sends out information about
    /// historic room key bundles we have received.
    historic_room_key_bundles_broadcaster: broadcast::Sender<RoomKeyBundleInfo>,
}

impl CryptoStoreWrapper {
    pub(crate) fn new(user_id: &UserId, device_id: &DeviceId, store: impl IntoCryptoStore) -> Self {
        let room_keys_received_sender = broadcast::Sender::new(10);
        let room_keys_withheld_received_sender = broadcast::Sender::new(10);
        let secrets_broadcaster = broadcast::Sender::new(10);
        // The identities broadcaster is responsible for user identities as well as
        // devices, that's why we increase the capacity here.
        let identities_broadcaster = broadcast::Sender::new(20);
        let historic_room_key_bundles_broadcaster = broadcast::Sender::new(10);

        Self {
            user_id: user_id.to_owned(),
            device_id: device_id.to_owned(),
            store: store.into_crypto_store(),
            sessions: SessionStore::new(),
            room_keys_received_sender,
            room_keys_withheld_received_sender,
            secrets_broadcaster,
            identities_broadcaster,
            historic_room_key_bundles_broadcaster,
        }
    }

    /// Save the set of changes to the store.
    ///
    /// Also responsible for sending updates to the broadcast streams such as
    /// `room_keys_received_sender` and `secrets_broadcaster`.
    ///
    /// # Arguments
    ///
    /// * `changes` - The set of changes that should be stored.
    pub async fn save_changes(&self, changes: Changes) -> store::Result<()> {
        let room_key_updates: Vec<_> =
            changes.inbound_group_sessions.iter().map(RoomKeyInfo::from).collect();

        let withheld_session_updates: Vec<_> = changes
            .withheld_session_info
            .iter()
            .flat_map(|(room_id, session_map)| {
                session_map.iter().map(|(session_id, withheld_event)| RoomKeyWithheldInfo {
                    room_id: room_id.to_owned(),
                    session_id: session_id.to_owned(),
                    withheld_event: withheld_event.clone(),
                })
            })
            .collect();

        // If our own identity verified status changes we need to do some checks on
        // other identities. So remember the verification status before
        // processing the changes
        let own_identity_was_verified_before_change = self
            .store
            .get_user_identity(self.user_id.as_ref())
            .await?
            .as_ref()
            .and_then(|i| i.own())
            .is_some_and(|own| own.is_verified());

        let secrets = changes.secrets.to_owned();
        let devices = changes.devices.to_owned();
        let identities = changes.identities.to_owned();
        let room_key_bundle_updates: Vec<_> =
            changes.received_room_key_bundles.iter().map(RoomKeyBundleInfo::from).collect();

        if devices
            .changed
            .iter()
            .any(|d| d.user_id() == self.user_id && d.device_id() == self.device_id)
        {
            // If our own device key changes, we need to clear the
            // session cache because the sessions contain a copy of our
            // device key.
            self.sessions.clear().await;
        } else {
            // Otherwise add the sessions to the cache.
            for session in &changes.sessions {
                self.sessions.add(session.clone()).await;
            }
        }

        self.store.save_changes(changes).await?;

        // If we updated our own public identity, log it for debugging purposes
        if tracing::level_enabled!(tracing::Level::DEBUG) {
            for updated_identity in
                identities.new.iter().chain(identities.changed.iter()).filter_map(|id| id.own())
            {
                let master_key = updated_identity.master_key().get_first_key();
                let user_signing_key = updated_identity.user_signing_key().get_first_key();
                let self_signing_key = updated_identity.self_signing_key().get_first_key();

                debug!(
                    ?master_key,
                    ?user_signing_key,
                    ?self_signing_key,
                    previously_verified = updated_identity.was_previously_verified(),
                    verified = updated_identity.is_verified(),
                    "Stored our own identity"
                );
            }
        }

        if !room_key_updates.is_empty() {
            // Ignore the result. It can only fail if there are no listeners.
            let _ = self.room_keys_received_sender.send(room_key_updates);
        }

        if !withheld_session_updates.is_empty() {
            let _ = self.room_keys_withheld_received_sender.send(withheld_session_updates);
        }

        for secret in secrets {
            let _ = self.secrets_broadcaster.send(secret);
        }

        for bundle_info in room_key_bundle_updates {
            let _ = self.historic_room_key_bundles_broadcaster.send(bundle_info);
        }

        if !devices.is_empty() || !identities.is_empty() {
            // Mapping the devices and user identities from the read-only variant to one's
            // that contain side-effects requires our own identity. This is
            // guaranteed to be up-to-date since we just persisted it.
            let maybe_own_identity =
                self.store.get_user_identity(&self.user_id).await?.and_then(|i| i.into_own());

            // If our identity was not verified before the change and is now, that means
            // this could impact the verification chain of other known
            // identities.
            if let Some(own_identity_after) = maybe_own_identity.as_ref() {
                // Only do this if our identity is passing from not verified to verified,
                // the previously_verified can only change in that case.
                let own_identity_is_verified = own_identity_after.is_verified();

                if !own_identity_was_verified_before_change && own_identity_is_verified {
                    debug!(
                        "Own identity is now verified, check all known identities for verification status changes"
                    );
                    // We need to review all the other identities to see if they are verified now
                    // and mark them as such
                    self.check_all_identities_and_update_was_previously_verified_flag_if_needed(
                        own_identity_after,
                    )
                    .await?;
                } else if own_identity_was_verified_before_change != own_identity_is_verified {
                    // Log that the verification state of the identity changed.
                    debug!(
                        own_identity_is_verified,
                        "The verification state of our own identity has changed",
                    );
                }
            }

            let _ = self.identities_broadcaster.send((maybe_own_identity, identities, devices));
        }

        Ok(())
    }

    async fn check_all_identities_and_update_was_previously_verified_flag_if_needed(
        &self,
        own_identity_after: &OwnUserIdentityData,
    ) -> Result<(), CryptoStoreError> {
        let tracked_users = self.store.load_tracked_users().await?;
        let mut updated_identities: Vec<UserIdentityData> = Default::default();
        for tracked_user in tracked_users {
            if let Some(other_identity) = self
                .store
                .get_user_identity(tracked_user.user_id.as_ref())
                .await?
                .as_ref()
                .and_then(|i| i.other())
                && !other_identity.was_previously_verified()
                && own_identity_after.is_identity_signed(other_identity)
            {
                trace!(?tracked_user.user_id, "Marking set verified_latch to true.");
                other_identity.mark_as_previously_verified();
                updated_identities.push(other_identity.clone().into());
            }
        }

        if !updated_identities.is_empty() {
            let identity_changes =
                IdentityChanges { changed: updated_identities, ..Default::default() };
            self.store
                .save_changes(Changes {
                    identities: identity_changes.clone(),
                    ..Default::default()
                })
                .await?;

            let _ = self.identities_broadcaster.send((
                Some(own_identity_after.clone()),
                identity_changes,
                DeviceChanges::default(),
            ));
        }

        Ok(())
    }

    pub async fn get_sessions(
        &self,
        sender_key: &str,
    ) -> store::Result<Option<Arc<Mutex<Vec<Session>>>>> {
        let sessions = self.sessions.get(sender_key).await;

        let sessions = if sessions.is_none() {
            let mut entries = self.sessions.entries.write().await;

            let sessions = entries.get(sender_key);

            if sessions.is_some() {
                sessions.cloned()
            } else {
                let sessions = self.store.get_sessions(sender_key).await?;
                let sessions = Arc::new(Mutex::new(sessions.unwrap_or_default()));

                entries.insert(sender_key.to_owned(), sessions.clone());

                Some(sessions)
            }
        } else {
            sessions
        };

        Ok(sessions)
    }

    /// Save a list of inbound group sessions to the store.
    ///
    /// # Arguments
    ///
    /// * `sessions` - The sessions to be saved.
    /// * `backed_up_to_version` - If the keys should be marked as having been
    ///   backed up, the version of the backup.
    ///
    /// Note: some implementations ignore `backup_version` and assume the
    /// current backup version, which is normally the same.
    pub async fn save_inbound_group_sessions(
        &self,
        sessions: Vec<InboundGroupSession>,
        backed_up_to_version: Option<&str>,
    ) -> store::Result<()> {
        let room_key_updates: Vec<_> = sessions.iter().map(RoomKeyInfo::from).collect();
        self.store.save_inbound_group_sessions(sessions, backed_up_to_version).await?;

        if !room_key_updates.is_empty() {
            // Ignore the result. It can only fail if there are no listeners.
            let _ = self.room_keys_received_sender.send(room_key_updates);
        }
        Ok(())
    }

    /// Receive notifications of room keys being received as a [`Stream`].
    ///
    /// Each time a room key is updated in any way, an update will be sent to
    /// the stream. Updates that happen at the same time are batched into a
    /// [`Vec`].
    ///
    /// If the reader of the stream lags too far behind an error will be sent to
    /// the reader.
    pub fn room_keys_received_stream(
        &self,
    ) -> impl Stream<Item = Result<Vec<RoomKeyInfo>, BroadcastStreamRecvError>> + use<> {
        BroadcastStream::new(self.room_keys_received_sender.subscribe())
    }

    /// Receive notifications of received `m.room_key.withheld` messages.
    ///
    /// Each time an `m.room_key.withheld` is received and stored, an update
    /// will be sent to the stream. Updates that happen at the same time are
    /// batched into a [`Vec`].
    ///
    /// If the reader of the stream lags too far behind, a warning will be
    /// logged and items will be dropped.
    pub fn room_keys_withheld_received_stream(
        &self,
    ) -> impl Stream<Item = Vec<RoomKeyWithheldInfo>> + use<> {
        let stream = BroadcastStream::new(self.room_keys_withheld_received_sender.subscribe());
        Self::filter_errors_out_of_stream(stream, "room_keys_withheld_received_stream")
    }

    /// Receive notifications of gossipped secrets being received and stored in
    /// the secret inbox as a [`Stream`].
    pub fn secrets_stream(&self) -> impl Stream<Item = GossippedSecret> + use<> {
        let stream = BroadcastStream::new(self.secrets_broadcaster.subscribe());
        Self::filter_errors_out_of_stream(stream, "secrets_stream")
    }

    /// Receive notifications of historic room key bundles being received and
    /// stored in the store as a [`Stream`].
    pub fn historic_room_key_stream(&self) -> impl Stream<Item = RoomKeyBundleInfo> + use<> {
        let stream = BroadcastStream::new(self.historic_room_key_bundles_broadcaster.subscribe());
        Self::filter_errors_out_of_stream(stream, "bundle_stream")
    }

    /// Returns a stream of newly created or updated cryptographic identities.
    ///
    /// This is just a helper method which allows us to build higher level
    /// device and user identity streams.
    pub(super) fn identities_stream(
        &self,
    ) -> impl Stream<Item = (Option<OwnUserIdentityData>, IdentityChanges, DeviceChanges)> + use<>
    {
        let stream = BroadcastStream::new(self.identities_broadcaster.subscribe());
        Self::filter_errors_out_of_stream(stream, "identities_stream")
    }

    /// Helper for *_stream functions: filters errors out of the stream,
    /// creating a new Stream.
    ///
    /// `BroadcastStream`s gives us `Result`s which can fail with
    /// `BroadcastStreamRecvError` if the reader falls behind. That's annoying
    /// to work with, so here we just emit a warning and drop the errors.
    fn filter_errors_out_of_stream<ItemType>(
        stream: BroadcastStream<ItemType>,
        stream_name: &str,
    ) -> impl Stream<Item = ItemType> + use<ItemType>
    where
        ItemType: 'static + Clone + Send,
    {
        let stream_name = stream_name.to_owned();
        stream.filter_map(move |result| {
            future::ready(match result {
                Ok(r) => Some(r),
                Err(BroadcastStreamRecvError::Lagged(lag)) => {
                    warn!("{stream_name} missed {lag} updates");
                    None
                }
            })
        })
    }

    /// Creates a [`CrossProcessLock`] for this store, that will contain the
    /// given key and value when hold.
    pub(crate) fn create_store_lock(
        &self,
        lock_key: String,
        lock_value: String,
    ) -> CrossProcessLock<LockableCryptoStore> {
        CrossProcessLock::new(LockableCryptoStore(self.store.clone()), lock_key, lock_value)
    }
}

impl Deref for CryptoStoreWrapper {
    type Target = DynCryptoStore;

    fn deref(&self) -> &Self::Target {
        self.store.deref()
    }
}

#[cfg(test)]
mod test {
    use matrix_sdk_test::async_test;
    use ruma::user_id;

    use super::*;
    use crate::machine::test_helpers::get_machine_pair_with_setup_sessions_test_helper;

    #[async_test]
    async fn test_cache_cleared_after_device_update() {
        let user_id = user_id!("@alice:example.com");
        let (first, second) =
            get_machine_pair_with_setup_sessions_test_helper(user_id, user_id, false).await;

        let sender_key = second.identity_keys().curve25519.to_base64();

        first
            .store()
            .inner
            .store
            .sessions
            .get(&sender_key)
            .await
            .expect("We should have a session in the cache.");

        let device_data = first
            .get_device(user_id, first.device_id(), None)
            .await
            .unwrap()
            .expect("We should have access to our own device.")
            .inner;

        // When we save a new version of our device keys
        first
            .store()
            .save_changes(Changes {
                devices: DeviceChanges { changed: vec![device_data], ..Default::default() },
                ..Default::default()
            })
            .await
            .unwrap();

        // Then the session is no longer in the cache
        assert!(
            first.store().inner.store.sessions.get(&sender_key).await.is_none(),
            "The session should no longer be in the cache after our own device keys changed"
        );
    }
}
