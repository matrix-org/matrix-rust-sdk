//!  Testing utilities - DO NOT USE IN PRODUCTION.

#![allow(dead_code)]

use matrix_sdk_base::SessionMeta;
use ruma::{api::MatrixVersion, device_id, user_id};

use crate::{
    config::RequestConfig,
    matrix_auth::{Session, SessionTokens},
    Client, ClientBuilder,
};

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
        meta: SessionMeta {
            user_id: user_id!("@example:localhost").to_owned(),
            device_id: device_id!("DEVICEID").to_owned(),
        },
        tokens: SessionTokens { access_token: "1234".to_owned(), refresh_token: None },
    };
    let client = no_retry_test_client(homeserver_url).await;
    client.matrix_auth().restore_session(session).await.unwrap();

    client
}
