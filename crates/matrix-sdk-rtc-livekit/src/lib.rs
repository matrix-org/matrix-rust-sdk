//! LiveKit SDK integration for matrix-sdk-rtc.

use async_trait::async_trait;
use matrix_sdk::Room as MatrixRoom;
use matrix_sdk_rtc::{LiveKitConnection, LiveKitConnector, LiveKitError, LiveKitResult};

pub use livekit;
pub use livekit::e2ee;
use livekit::id::ParticipantIdentity;
use livekit::RoomEvent;
pub use livekit::{ConnectionState, Room, RoomOptions};
use tokio::sync::Mutex;
use tracing::info;

#[cfg(feature = "crypto")]
pub mod matrix_keys {
    use async_trait::async_trait;
    use matrix_sdk::ruma::RoomId;
    use matrix_sdk::Room as MatrixRoom;
    use matrix_sdk_crypto::{
        olm::ExportedRoomKey, store::CryptoStoreError, types::room_history::RoomKeyBundle,
        OlmMachine,
    };
    use thiserror::Error;

    /// Errors returned while exporting Matrix key material.
    #[derive(Debug, Error)]
    pub enum MatrixKeyMaterialError {
        /// The Matrix SDK crypto machine is not initialized.
        #[error("missing olm machine; crypto not initialized")]
        NoOlmMachine,
        /// The crypto store returned an error while exporting keys.
        #[error(transparent)]
        CryptoStore(#[from] CryptoStoreError),
    }

    /// Provides Matrix room key material for E2EE integrations.
    #[async_trait]
    pub trait MatrixKeyMaterialProvider: Send + Sync {
        /// Export the key material for the given Matrix room.
        async fn export_room_keys(
            &self,
            room: &MatrixRoom,
        ) -> Result<Vec<ExportedRoomKey>, MatrixKeyMaterialError>;
    }

    /// Provides per-participant key bundles for a Matrix room (MSC4268).
    #[async_trait]
    pub trait PerParticipantKeyMaterialProvider: Send + Sync {
        /// Build a per-participant key bundle for the room.
        async fn per_participant_key_bundle(
            &self,
            room_id: &RoomId,
        ) -> Result<RoomKeyBundle, MatrixKeyMaterialError>;
    }

    /// Matrix key material provider backed by an `OlmMachine`.
    #[derive(Debug, Clone)]
    pub struct OlmMachineKeyMaterialProvider {
        olm_machine: OlmMachine,
    }

    impl OlmMachineKeyMaterialProvider {
        /// Create a provider from an initialized `OlmMachine`.
        pub fn new(olm_machine: OlmMachine) -> Self {
            Self { olm_machine }
        }
    }

    #[async_trait]
    impl MatrixKeyMaterialProvider for OlmMachineKeyMaterialProvider {
        async fn export_room_keys(
            &self,
            room: &MatrixRoom,
        ) -> Result<Vec<ExportedRoomKey>, MatrixKeyMaterialError> {
            let room_id = room.room_id();
            let keys = self
                .olm_machine
                .store()
                .export_room_keys(|session| session.room_id() == room_id)
                .await?;
            Ok(keys)
        }
    }

    #[async_trait]
    impl PerParticipantKeyMaterialProvider for OlmMachineKeyMaterialProvider {
        async fn per_participant_key_bundle(
            &self,
            room_id: &RoomId,
        ) -> Result<RoomKeyBundle, MatrixKeyMaterialError> {
            let bundle = self.olm_machine.store().build_room_key_bundle(room_id).await?;
            Ok(bundle)
        }
    }

    /// Fetch the current `OlmMachine` for a room's client.
    pub async fn room_olm_machine(room: &MatrixRoom) -> Result<OlmMachine, MatrixKeyMaterialError> {
        let client = room.client();
        let olm_machine = client.olm_machine().await;
        let Some(olm_machine) = olm_machine.as_ref() else {
            return Err(MatrixKeyMaterialError::NoOlmMachine);
        };
        Ok(olm_machine.clone())
    }

    #[cfg(test)]
    mod tests {
        use std::sync::Arc;

        use matrix_sdk::ruma::{device_id, room_id, user_id};
        use matrix_sdk_crypto::{store::MemoryStore, OlmMachine};

        use super::{OlmMachineKeyMaterialProvider, PerParticipantKeyMaterialProvider};

        #[tokio::test]
        async fn per_participant_key_bundle_empty_without_sessions() {
            let user_id = user_id!("@alice:example.org");
            let device_id = device_id!("DEVICEID");
            let store = Arc::new(MemoryStore::new());
            let olm_machine = OlmMachine::with_store(user_id, device_id, store, None)
                .await
                .expect("create olm machine");
            let provider = OlmMachineKeyMaterialProvider::new(olm_machine);
            let room_id = room_id!("!room:example.org");

            let bundle = provider.per_participant_key_bundle(room_id).await.unwrap();

            assert!(bundle.is_empty());
        }
    }
}

/// A token provider for joining LiveKit rooms.
#[async_trait]
pub trait LiveKitTokenProvider: Send + Sync {
    /// Provide a LiveKit access token for the given Matrix room.
    async fn token(&self, room: &MatrixRoom) -> LiveKitResult<String>;
}

/// Provides LiveKit room options.
pub trait LiveKitRoomOptionsProvider: Send + Sync {
    /// Create the LiveKit room options used when connecting to LiveKit.
    fn room_options(&self) -> RoomOptions;
}

impl<F> LiveKitRoomOptionsProvider for F
where
    F: Fn() -> RoomOptions + Send + Sync,
{
    fn room_options(&self) -> RoomOptions {
        (self)()
    }
}

/// A LiveKit connection backed by the LiveKit Rust SDK.
#[derive(Debug)]
pub struct LiveKitSdkConnection {
    room: Room,
    events: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<RoomEvent>>>,
}

impl LiveKitSdkConnection {
    /// Access the underlying LiveKit room handle.
    pub fn room(&self) -> &Room {
        &self.room
    }

    /// Consume and return the LiveKit room event stream.
    ///
    /// Returns `None` if the stream has already been taken.
    pub async fn take_events(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<RoomEvent>> {
        self.events.lock().await.take()
    }

    /// Consume this connection and return the underlying LiveKit room.
    pub fn into_room(self) -> Room {
        self.room
    }
}

#[async_trait]
impl LiveKitConnection for LiveKitSdkConnection {
    async fn disconnect(self) -> LiveKitResult<()> {
        drop(self);
        Ok(())
    }
}

/// Connector implementation that joins rooms using the LiveKit Rust SDK.
#[derive(Debug)]
pub struct LiveKitSdkConnector<T, O> {
    token_provider: T,
    room_options: O,
}

impl<T, O> LiveKitSdkConnector<T, O> {
    /// Create a new connector using the provided token provider and room options.
    pub fn new(token_provider: T, room_options: O) -> Self {
        Self { token_provider, room_options }
    }

    /// Access the configured token provider.
    pub fn token_provider(&self) -> &T {
        &self.token_provider
    }

    /// Access the configured room options provider.
    pub fn room_options_provider(&self) -> &O {
        &self.room_options
    }
}

#[async_trait]
impl<T, O> LiveKitConnector for LiveKitSdkConnector<T, O>
where
    T: LiveKitTokenProvider,
    O: LiveKitRoomOptionsProvider,
{
    type Connection = LiveKitSdkConnection;

    async fn connect(
        &self,
        service_url: &str,
        room: &MatrixRoom,
    ) -> LiveKitResult<Self::Connection> {
        let token = self.token_provider.token(room).await?;
        let mut room_options = self.room_options_provider().room_options();
        if room_options.encryption.is_none() {
            room_options.encryption = room_options.e2ee.clone();
        }

        if let Some(encryption) = room_options.encryption.as_ref() {
            let key_provider = &encryption.key_provider;
            let key_index = key_provider.get_latest_key_index();
            let maybe_local_key = room
                .client()
                .user_id()
                .zip(room.client().device_id())
                .map(|(user_id, device_id)| ParticipantIdentity(format!("{user_id}:{device_id}")))
                .and_then(|identity| key_provider.get_key(&identity, key_index));
            info!(
                room_id = ?room.room_id(),
                key_index,
                key_provider_key = ?maybe_local_key,
                "resolved LiveKit encryption key_provider key before connect"
            );
        }
        info!(
            room_id = ?room.room_id(),
            service_url,
            room_options = ?room_options,
            "connecting to LiveKit room with resolved room options"
        );
        let (livekit_room, events) = Room::connect(service_url, &token, room_options)
            .await
            .map_err(LiveKitError::connector)?;
        Ok(LiveKitSdkConnection { room: livekit_room, events: Mutex::new(Some(events)) })
    }
}
