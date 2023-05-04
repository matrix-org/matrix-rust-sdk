use matrix_sdk_base::sync::SyncResponse;
use ruma::api::client::sync::sync_events::v4;
use tracing::{debug, instrument};

use super::{SlidingSync, SlidingSyncBuilder};
use crate::{Client, Result};

impl Client {
    /// Create a [`SlidingSyncBuilder`] tied to this client.
    pub async fn sliding_sync(&self) -> SlidingSyncBuilder {
        SlidingSync::builder(self.clone())
    }

    #[instrument(skip(self, response))]
    pub(crate) async fn process_sliding_sync(
        &self,
        response: &v4::Response,
    ) -> Result<SyncResponse> {
        let response = self.base_client().process_sliding_sync(response).await?;
        debug!("done processing on base_client");
        self.handle_sync_response(&response).await?;

        Ok(response)
    }
}
