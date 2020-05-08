use matrix_sdk::{
    events::room::message::{MessageEventContent, TextMessageEventContent},
    identifiers::RoomId,
    Client, ClientConfig,
};
use std::convert::TryFrom;
use url::Url;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub async fn run() -> Result<JsValue, JsValue> {
    let homeserver_url = "http://localhost:8008";
    let username = "user";
    let password = "password";

    let client_config = ClientConfig::new();
    let homeserver_url = Url::parse(&homeserver_url).unwrap();
    let client = Client::new_with_config(homeserver_url, None, client_config).unwrap();

    client
        .login(username, password, None, Some("rust-sdk"))
        .await
        .unwrap();

    let room_id = RoomId::try_from("!KpLWMcXcHKDMfEYNqA:localhost").unwrap();

    let content = MessageEventContent::Text(TextMessageEventContent {
        body: "hello from wasm".to_string(),
        format: None,
        formatted_body: None,
        relates_to: None,
    });

    client.room_send(&room_id, content, None).await.unwrap();

    Ok(JsValue::NULL)
}
