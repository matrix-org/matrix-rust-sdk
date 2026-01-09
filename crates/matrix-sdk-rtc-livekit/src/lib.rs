//! LiveKit SDK integration for matrix-sdk-rtc.

use async_trait::async_trait;
use matrix_sdk::Room as MatrixRoom;
use matrix_sdk_rtc::{LiveKitConnection, LiveKitConnector, LiveKitError, LiveKitResult};

pub use livekit::e2ee::{ExternalKeyProvider, KeyProvider};
pub use livekit::{ConnectionState, Room, RoomOptions};

/// Alias for the LiveKit base key provider.
pub type BaseKeyProvider = KeyProvider;

/// Alias for the LiveKit external E2EE key provider.
pub type ExternalE2EEKeyProvider = ExternalKeyProvider;

/// A token provider for joining LiveKit rooms.
#[async_trait]
pub trait LiveKitTokenProvider: Send + Sync {
    /// Provide a LiveKit access token for the given Matrix room.
    async fn token(&self, room: &MatrixRoom) -> LiveKitResult<String>;
}

/// Provides LiveKit room options for a Matrix room.
pub trait LiveKitRoomOptionsProvider: Send + Sync {
    /// Create the LiveKit room options for the given Matrix room.
    fn room_options(&self, room: &MatrixRoom) -> RoomOptions;
}

impl<F> LiveKitRoomOptionsProvider for F
where
    F: Fn(&MatrixRoom) -> RoomOptions + Send + Sync,
{
    fn room_options(&self, room: &MatrixRoom) -> RoomOptions {
        (self)(room)
    }
}

/// A LiveKit connection backed by the LiveKit Rust SDK.
#[derive(Debug)]
pub struct LiveKitSdkConnection {
    room: Room,
}

impl LiveKitSdkConnection {
    /// Access the underlying LiveKit room handle.
    pub fn room(&self) -> &Room {
        &self.room
    }

    /// Consume this connection and return the underlying LiveKit room.
    pub fn into_room(self) -> Room {
        self.room
    }
}

#[async_trait]
impl LiveKitConnection for LiveKitSdkConnection {
    async fn disconnect(self) -> LiveKitResult<()> {
        self.room.disconnect().await.map_err(LiveKitError::connector)
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

    async fn connect(&self, service_url: &str, room: &MatrixRoom) -> LiveKitResult<Self::Connection> {
        let token = self.token_provider.token(room).await?;
        let room_options = self.room_options.room_options(room);
        let livekit_room = Room::connect(service_url, token, room_options)
            .await
            .map_err(LiveKitError::connector)?;
        Ok(LiveKitSdkConnection { room: livekit_room })
    }
}
