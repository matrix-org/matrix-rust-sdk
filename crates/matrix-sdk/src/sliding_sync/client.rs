use matrix_sdk_base::sync::SyncResponse;
use ruma::{api::client::sync::sync_events::v4, events::AnyToDeviceEvent, serde::Raw};
use tracing::{debug, instrument};

use super::{SlidingSync, SlidingSyncBuilder};
use crate::{Client, Result};

impl Client {
    /// Create a [`SlidingSyncBuilder`] tied to this client, with the given
    /// identifier.
    ///
    /// Note: the identifier must not be more than 16 chars long!
    pub fn sliding_sync(&self, id: impl Into<String>) -> Result<SlidingSyncBuilder> {
        Ok(SlidingSync::builder(id.into(), self.clone())?)
    }

    /// Handle all the e2ee information provided in a sliding sync response.
    #[cfg(feature = "e2e-encryption")]
    pub(crate) async fn process_sliding_sync_e2ee(
        &self,
        extensions: &v4::Extensions,
    ) -> Result<Vec<Raw<AnyToDeviceEvent>>> {
        Ok(self.base_client().process_sliding_sync_e2ee(extensions).await?)
    }

    /// Handle all the information provided in a sliding sync response, except
    /// for the e2ee bits that are handled by `process_sliding_sync_e2ee`
    /// (and which results can be passed as the second argument).
    #[instrument(skip(self, response))]
    pub async fn process_sliding_sync(
        &self,
        response: &v4::Response,
        to_device_events: Vec<Raw<AnyToDeviceEvent>>,
    ) -> Result<SyncResponse> {
        let mut response = self.base_client().process_sliding_sync(response).await?;

        response.to_device.extend(to_device_events);

        debug!("done processing on base_client");
        self.handle_sync_response(&response).await?;

        Ok(response)
    }
}
