use std::{ops::Deref, sync::Arc};

use futures_core::Stream;
use futures_util::StreamExt;
use ruma::{OwnedUserId, UserId};
use tokio::sync::broadcast;
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tracing::warn;

use crate::{
    store,
    store::{locks::CryptoStoreLock, Changes, DynCryptoStore, IntoCryptoStore, RoomKeyInfo},
    GossippedSecret,
};

/// A wrapper for crypto store implementations that adds update notifiers.
///
/// This is shared between [`StoreInner`] and
/// [`crate::verification::VerificationStore`].
#[derive(Debug)]
pub(crate) struct CryptoStoreWrapper {
    user_id: OwnedUserId,
    store: Arc<DynCryptoStore>,

    /// The sender side of a broadcast stream that is notified whenever we get
    /// an update to an inbound group session.
    room_keys_received_sender: broadcast::Sender<Vec<RoomKeyInfo>>,

    /// The sender side of a broadcast channel which sends out secrets we
    /// received as a `m.secret.send` event.
    secrets_broadcaster: broadcast::Sender<GossippedSecret>,
}

impl CryptoStoreWrapper {
    pub(crate) fn new(user_id: &UserId, store: impl IntoCryptoStore) -> Self {
        let room_keys_received_sender = broadcast::Sender::new(10);
        let secrets_broadcaster = broadcast::Sender::new(10);

        Self {
            user_id: user_id.to_owned(),
            store: store.into_crypto_store(),
            room_keys_received_sender,
            secrets_broadcaster,
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

        let secrets = changes.secrets.to_owned();

        self.store.save_changes(changes).await?;

        if !room_key_updates.is_empty() {
            // Ignore the result. It can only fail if there are no listeners.
            let _ = self.room_keys_received_sender.send(room_key_updates);
        }

        for secret in secrets {
            let _ = self.secrets_broadcaster.send(secret);
        }

        Ok(())
    }

    /// Receive notifications of room keys being received as a [`Stream`].
    ///
    /// Each time a room key is updated in any way, an update will be sent to
    /// the stream. Updates that happen at the same time are batched into a
    /// [`Vec`].
    ///
    /// If the reader of the stream lags too far behind, a warning will be
    /// logged and items will be dropped.
    pub fn room_keys_received_stream(&self) -> impl Stream<Item = Vec<RoomKeyInfo>> {
        let stream = BroadcastStream::new(self.room_keys_received_sender.subscribe());

        // the raw BroadcastStream gives us Results which can fail with
        // BroadcastStreamRecvError if the reader falls behind. That's annoying to work
        // with, so here we just drop the errors.
        stream.filter_map(|result| async move {
            match result {
                Ok(r) => Some(r),
                Err(BroadcastStreamRecvError::Lagged(lag)) => {
                    warn!("room_keys_received_stream missed {lag} updates");
                    None
                }
            }
        })
    }

    /// Receive notifications of gossipped secrets being received and stored in
    /// the secret inbox as a [`Stream`].
    pub fn secrets_stream(&self) -> impl Stream<Item = GossippedSecret> {
        let stream = BroadcastStream::new(self.secrets_broadcaster.subscribe());

        // the raw BroadcastStream gives us Results which can fail with
        // BroadcastStreamRecvError if the reader falls behind. That's annoying to work
        // with, so here we just drop the errors.
        stream.filter_map(|result| async move {
            match result {
                Ok(r) => Some(r),
                Err(BroadcastStreamRecvError::Lagged(lag)) => {
                    warn!("secrets_stream missed {lag} updates");
                    None
                }
            }
        })
    }

    /// Creates a `CryptoStoreLock` for this store, that will contain the given
    /// key and value when held.
    pub fn create_store_lock(&self, lock_key: String, lock_value: String) -> CryptoStoreLock {
        CryptoStoreLock::new(self.store.clone(), lock_key, lock_value)
    }
}

impl Deref for CryptoStoreWrapper {
    type Target = DynCryptoStore;

    fn deref(&self) -> &Self::Target {
        self.store.deref()
    }
}
