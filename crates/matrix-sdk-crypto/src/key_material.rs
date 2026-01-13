//! Key material helpers for integrating Matrix E2EE with external systems.

use futures_core::Stream;
use ruma::{RoomId, UserId};

use crate::{
    OlmMachine,
    olm::{ExportedRoomKey, InboundGroupSession},
    store::{CryptoStoreError, Result as StoreResult, types::{RoomKeyBundleInfo, StoredRoomKeyBundleData}},
    types::room_history::RoomKeyBundle,
};

/// Convenience wrapper to access Matrix key material for external E2EE systems.
#[derive(Clone, Debug)]
pub struct MatrixKeyMaterialProvider {
    machine: OlmMachine,
}

impl MatrixKeyMaterialProvider {
    /// Create a new provider backed by the given [`OlmMachine`].
    pub fn new(machine: OlmMachine) -> Self {
        Self { machine }
    }

    /// Access the underlying [`OlmMachine`].
    pub fn machine(&self) -> &OlmMachine {
        &self.machine
    }

    /// Export room keys that match the given predicate.
    pub async fn export_room_keys(
        &self,
        predicate: impl FnMut(&InboundGroupSession) -> bool,
    ) -> StoreResult<Vec<ExportedRoomKey>> {
        self.machine.store().export_room_keys(predicate).await
    }

    /// Stream room keys that match the given predicate.
    pub async fn export_room_keys_stream(
        &self,
        predicate: impl FnMut(&InboundGroupSession) -> bool,
    ) -> StoreResult<impl Stream<Item = ExportedRoomKey>> {
        self.machine.store().export_room_keys_stream(predicate).await
    }

    /// Build a room key bundle for sharing encrypted history (MSC4268).
    pub async fn build_room_key_bundle(
        &self,
        room_id: &RoomId,
    ) -> Result<RoomKeyBundle, CryptoStoreError> {
        self.machine.store().build_room_key_bundle(room_id).await
    }

    /// Stream notifications about received historic room key bundles.
    pub fn historic_room_key_stream(&self) -> impl Stream<Item = RoomKeyBundleInfo> + use<> {
        self.machine.store().historic_room_key_stream()
    }

    /// Retrieve stored metadata for a received room key bundle.
    pub async fn get_received_room_key_bundle_data(
        &self,
        room_id: &RoomId,
        sender: &UserId,
    ) -> StoreResult<Option<StoredRoomKeyBundleData>> {
        self.machine.store().get_received_room_key_bundle_data(room_id, sender).await
    }

    /// Import a decrypted room key bundle into the store.
    pub async fn receive_room_key_bundle(
        &self,
        bundle_info: &StoredRoomKeyBundleData,
        bundle: RoomKeyBundle,
        progress_listener: impl Fn(usize, usize),
    ) -> Result<(), CryptoStoreError> {
        self.machine.store().receive_room_key_bundle(bundle_info, bundle, progress_listener).await
    }
}
