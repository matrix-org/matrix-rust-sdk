use matrix_sdk_base::sync::SyncResponse;
use ruma::api::client::sync::sync_events::v4;
use tracing::{debug, instrument};

use super::{SlidingSync, SlidingSyncListLoopBuilder, SlidingSyncNotificationLoopBuilder};
use crate::{Client, Result};

impl Client {
    /// Create a [`SlidingSyncListLoopBuilder`] tied to this client.
    pub fn sliding_sync_list_loop(&self) -> SlidingSyncListLoopBuilder {
        SlidingSync::new_list_loop(self.clone())
    }

    /// Create a [`SlidingSyncNotificationLoopBuilder`] tied to this client.
    pub fn sliding_sync_notification_loop(&self) -> SlidingSyncNotificationLoopBuilder {
        SlidingSync::new_notification_loop(self.clone())
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
