use std::collections::BTreeMap;

use imbl::Vector;
use matrix_sdk_base::{sync::SyncResponse, PreviousEventsProvider};
use ruma::{api::client::sync::sync_events::v4, events::AnyToDeviceEvent, serde::Raw, OwnedRoomId};

use super::{SlidingSync, SlidingSyncBuilder};
use crate::{Client, Result, SlidingSyncRoom};

impl Client {
    /// Create a [`SlidingSyncBuilder`] tied to this client, with the given
    /// identifier.
    ///
    /// Note: the identifier must not be more than 16 chars long!
    pub fn sliding_sync(&self, id: impl Into<String>) -> Result<SlidingSyncBuilder> {
        Ok(SlidingSync::builder(id.into(), self.clone())?)
    }

    /// Handle all the information provided in a sliding sync response, except
    /// for the e2ee bits.
    ///
    /// If you need to handle encryption too, use the internal
    /// `SlidingSyncResponseProcessor` instead.
    #[cfg(any(test, feature = "testing"))]
    #[tracing::instrument(skip(self, response))]
    pub async fn process_sliding_sync_test_helper(
        &self,
        response: &v4::Response,
    ) -> Result<SyncResponse> {
        let response = self.base_client().process_sliding_sync(response, &()).await?;

        tracing::debug!("done processing on base_client");
        self.call_sync_response_handlers(&response).await?;

        Ok(response)
    }
}

struct SlidingSyncPreviousEventsProvider<'a>(&'a BTreeMap<OwnedRoomId, SlidingSyncRoom>);

impl<'a> PreviousEventsProvider for SlidingSyncPreviousEventsProvider<'a> {
    fn for_room(
        &self,
        room_id: &ruma::RoomId,
    ) -> Vector<matrix_sdk_common::deserialized_responses::SyncTimelineEvent> {
        self.0.get(room_id).map(|room| room.timeline_queue()).unwrap_or_default()
    }
}

/// Small helper to handle a `SlidingSync` response's sub parts.
///
/// This will properly handle the encryption and the room response
/// independently, if needs be, making sure that both are properly processed by
/// event handlers.
#[must_use]
pub(crate) struct SlidingSyncResponseProcessor<'a> {
    client: Client,
    to_device_events: Vec<Raw<AnyToDeviceEvent>>,
    response: Option<SyncResponse>,
    rooms: &'a BTreeMap<OwnedRoomId, SlidingSyncRoom>,
}

impl<'a> SlidingSyncResponseProcessor<'a> {
    pub fn new(client: Client, rooms: &'a BTreeMap<OwnedRoomId, SlidingSyncRoom>) -> Self {
        Self { client, to_device_events: Vec::new(), response: None, rooms }
    }

    #[cfg(feature = "e2e-encryption")]
    pub async fn handle_encryption(&mut self, extensions: &v4::Extensions) -> Result<()> {
        // This is an internal API misuse if this is triggered (calling
        // handle_room_response before this function), so panic is fine.
        assert!(self.response.is_none());

        self.to_device_events =
            self.client.base_client().process_sliding_sync_e2ee(extensions).await?;

        // Some new keys might have been received, so trigger a backup if needed.
        self.client.encryption().backups().maybe_trigger_backup();

        Ok(())
    }

    pub async fn handle_room_response(&mut self, response: &v4::Response) -> Result<()> {
        self.response = Some(
            self.client
                .base_client()
                .process_sliding_sync(response, &SlidingSyncPreviousEventsProvider(self.rooms))
                .await?,
        );
        Ok(())
    }

    pub async fn process_and_take_response(mut self) -> Result<SyncResponse> {
        let mut response = self.response.take().unwrap_or_default();

        response.to_device.extend(self.to_device_events);

        self.client.call_sync_response_handlers(&response).await?;

        Ok(response)
    }
}
