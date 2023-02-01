//!  Testing utilities - DO NOT USE IN PRODUCTION.

#![allow(dead_code)]
use matrix_sdk_base::Session;
use ruma::{api::MatrixVersion, device_id, user_id};

#[cfg(feature = "experimental-sliding-sync")]
use crate::sliding_sync::SlidingSync;
use crate::{config::RequestConfig, Client, ClientBuilder};

pub(crate) fn test_client_builder(homeserver_url: Option<String>) -> ClientBuilder {
    let homeserver = homeserver_url.as_deref().unwrap_or("http://localhost:1234");
    Client::builder().homeserver_url(homeserver).server_versions([MatrixVersion::V1_0])
}

pub(crate) async fn no_retry_test_client(homeserver_url: Option<String>) -> Client {
    test_client_builder(homeserver_url)
        .request_config(RequestConfig::new().disable_retry())
        .build()
        .await
        .unwrap()
}

pub(crate) async fn logged_in_client(homeserver_url: Option<String>) -> Client {
    let session = Session {
        access_token: "1234".to_owned(),
        refresh_token: None,
        user_id: user_id!("@example:localhost").to_owned(),
        device_id: device_id!("DEVICEID").to_owned(),
    };
    let client = no_retry_test_client(homeserver_url).await;
    client.restore_session(session).await.unwrap();

    client
}

/// Force a specific pos-value to be used for the given sliding-sync instance.
#[cfg(feature = "experimental-sliding-sync")]
pub fn force_sliding_sync_pos(sliding_sync: &SlidingSync, new_pos: String) {
    sliding_sync.pos.set(Some(new_pos));
}
