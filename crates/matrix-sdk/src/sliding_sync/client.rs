use matrix_sdk_base::sync::SyncResponse;
use ruma::api::client::sync::sync_events::v4;
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

    /// Handle all the information provided in a sliding sync response
    #[instrument(skip(self, response))]
    pub async fn process_sliding_sync(&self, response: &v4::Response) -> Result<SyncResponse> {
        let response = self.base_client().process_sliding_sync(response).await?;
        debug!("done processing on base_client");
        self.handle_sync_response(&response).await?;

        Ok(response)
    }
}
