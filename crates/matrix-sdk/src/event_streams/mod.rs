//! Core support for MSC4471 event streams.
//!
//! Event streams let a client publish short-lived, device-scoped updates for a
//! room message without committing each intermediate update to room history.
//! This module keeps that state in memory and exposes publisher and subscriber
//! handles around the Ruma MSC4471 event content types.

mod publisher;
mod subscription;

use std::collections::BTreeMap;

use ruma::{
    DeviceId, OwnedEventId, OwnedRoomId, TransactionId, UserId,
    api::client::to_device::send_event_to_device::v3::Request as ToDeviceRequest,
    events::{AnyToDeviceEventContent, ToDeviceEventContent},
    serde::Raw,
    to_device::DeviceIdOrAllDevices,
};
use thiserror::Error;
use tokio::sync::broadcast;

pub use self::{
    publisher::{EventStreamPublisher, EventStreamPublisherOptions, EventStreamPublishers},
    subscription::{
        EventStreamSubscriberUpdate, EventStreamSubscription, EventStreamSubscriptions,
    },
};
use crate::{Client, HttpError, room::edit::EditError};

/// A specialized result for event stream operations.
pub type Result<T, E = EventStreamError> = std::result::Result<T, E>;

/// An error returned by an event stream operation.
#[derive(Debug, Error)]
pub enum EventStreamError {
    /// The operation needs the current device ID, but the client is not logged
    /// in.
    #[error("event streams require a logged-in client")]
    AuthenticationRequired,

    /// The requested stream is not active in this client's in-memory state.
    #[error("unknown event stream")]
    UnknownStream,

    /// The descriptor event is not an unredacted room message with a stream
    /// descriptor.
    #[error("stream descriptor event is not an unredacted room message with a stream descriptor")]
    InvalidDescriptorEvent,

    /// The descriptor event does not have a sender.
    #[error("stream descriptor event does not have a sender")]
    MissingDescriptorSender,

    /// An HTTP request failed.
    #[error(transparent)]
    Http(#[from] HttpError),

    /// A matrix-sdk operation failed.
    #[error(transparent)]
    Sdk(#[from] crate::Error),

    /// Creating a final edit event failed.
    #[error(transparent)]
    Edit(#[from] EditError),

    /// Serializing a to-device payload failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// The event stream update receiver lagged.
    #[error("event stream update receiver lagged")]
    Lagged,
}

impl From<broadcast::error::RecvError> for EventStreamError {
    fn from(value: broadcast::error::RecvError) -> Self {
        match value {
            broadcast::error::RecvError::Closed => Self::UnknownStream,
            broadcast::error::RecvError::Lagged(_) => Self::Lagged,
        }
    }
}

/// Identifies a stream by the room event that advertised it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StreamId {
    /// The room containing the descriptor event.
    pub room_id: OwnedRoomId,

    /// The message event that advertised the stream.
    pub event_id: OwnedEventId,
}

impl StreamId {
    /// Create a stream identifier from its descriptor event.
    pub fn new(room_id: OwnedRoomId, event_id: OwnedEventId) -> Self {
        Self { room_id, event_id }
    }
}

/// Client-owned namespace for publishing and subscribing to event streams.
#[derive(Clone, Debug)]
pub struct EventStreams {
    publishers: EventStreamPublishers,
    subscriptions: EventStreamSubscriptions,
}

impl EventStreams {
    pub(crate) fn new(client: Client) -> Self {
        let publishers = EventStreamPublishers::new(client.clone());
        let subscriptions = EventStreamSubscriptions::new(client);

        Self { publishers, subscriptions }
    }

    /// Access publisher-side event stream operations.
    pub fn publishers(&self) -> EventStreamPublishers {
        self.publishers.clone()
    }

    /// Access subscriber-side event stream operations.
    pub fn subscriptions(&self) -> EventStreamSubscriptions {
        self.subscriptions.clone()
    }
}

/// Send one typed to-device event to one specific device.
async fn send_to_device<C>(
    client: &Client,
    user_id: &UserId,
    device_id: &DeviceId,
    content: C,
) -> Result<()>
where
    C: ToDeviceEventContent,
{
    let event_type = content.event_type();
    let messages = BTreeMap::from([(
        user_id.to_owned(),
        BTreeMap::from([(
            DeviceIdOrAllDevices::DeviceId(device_id.to_owned()),
            raw_content(&content)?,
        )]),
    )]);

    let request = ToDeviceRequest::new_raw(event_type, TransactionId::new(), messages);
    client.send(request).await?;

    Ok(())
}

fn raw_content<C>(content: &C) -> Result<Raw<AnyToDeviceEventContent>>
where
    C: ToDeviceEventContent,
{
    Ok(Raw::new(content)?.cast_unchecked())
}
