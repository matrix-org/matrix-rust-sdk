//! `RoomList` API.

use matrix_sdk::{Client, Result, SlidingSync, SlidingSyncList};

#[derive(Debug)]
pub struct RoomList {
    sliding_sync: SlidingSync,
}

impl RoomList {
    pub async fn new(client: Client) -> Result<Self> {
        Ok(Self {
            sliding_sync: client
                .sliding_sync()
                .storage_key(Some("matrix-sdk-ui-roomlist".to_string()))
                .add_cached_list(SlidingSyncList::builder("all_rooms"))
                .await?
                .add_list(SlidingSyncList::builder("visible_rooms"))
                .build()
                .await?,
        })
    }

    pub fn sync(&self) {}
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn test_has_two_lists() {
        // let
    }
}
