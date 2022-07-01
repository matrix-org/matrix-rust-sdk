use matrix_sdk::{config::RequestConfig, Client, ClientBuilder, Session};
use ruma::{api::MatrixVersion, device_id, user_id};
use url::Url;

mod client;
mod room;

fn test_client_builder() -> ClientBuilder {
    let homeserver = Url::parse(&mockito::server_url()).unwrap();
    Client::builder().homeserver_url(homeserver).server_versions([MatrixVersion::V1_0])
}

async fn no_retry_test_client() -> Client {
    test_client_builder()
        .request_config(RequestConfig::new().disable_retry())
        .build()
        .await
        .unwrap()
}

async fn logged_in_client() -> Client {
    let session = Session {
        access_token: "1234".to_owned(),
        user_id: user_id!("@example:localhost").to_owned(),
        device_id: device_id!("DEVICEID").to_owned(),
    };
    let client = no_retry_test_client().await;
    client.restore_login(session).await.unwrap();

    client
}
