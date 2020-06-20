use matrix_sdk::{
    api::r0::sync::sync_events::Response as SyncResponse,
    events::room::message::{MessageEventContent, TextMessageEventContent},
    events::MessageEventStub,
    identifiers::RoomId,
    Client, ClientConfig, SyncSettings,
};
use url::Url;
use wasm_bindgen::prelude::*;
use web_sys::console;

struct WasmBot(Client);

impl WasmBot {
    async fn on_room_message(
        &self,
        room_id: &RoomId,
        event: MessageEventStub<MessageEventContent>,
    ) {
        let msg_body = if let MessageEventStub {
            content: MessageEventContent::Text(TextMessageEventContent { body: msg_body, .. }),
            ..
        } = event
        {
            msg_body.clone()
        } else {
            return;
        };

        console::log_1(&format!("Received message event {:?}", &msg_body).into());

        if msg_body.starts_with("!party") {
            let content = MessageEventContent::Text(TextMessageEventContent::new_plain(
                "🎉🎊🥳 let's PARTY with wasm!! 🥳🎊🎉".to_string(),
            ));

            self.0.room_send(&room_id, content, None).await.unwrap();
        }
    }
    async fn on_sync_response(&self, response: SyncResponse) {
        console::log_1(&format!("Synced").into());

        for (room_id, room) in response.rooms.join {
            for event in room.timeline.events {
                if let Ok(event) = event.deserialize() {
                    self.on_room_message(&room_id, event).await
                }
            }
        }
    }
}

#[wasm_bindgen]
pub async fn run() -> Result<JsValue, JsValue> {
    let homeserver_url = "http://localhost:8008";
    let username = "user";
    let password = "password";

    let client_config = ClientConfig::new();
    let homeserver_url = Url::parse(&homeserver_url).unwrap();
    let client = Client::new_with_config(homeserver_url, client_config).unwrap();

    client
        .login(username, password, None, Some("rust-sdk-wasm"))
        .await
        .unwrap();

    let bot = WasmBot(client.clone());

    client.sync(SyncSettings::default()).await.unwrap();

    let settings = SyncSettings::default().token(client.sync_token().await.unwrap());
    client
        .sync_forever(settings, |response| bot.on_sync_response(response))
        .await;

    Ok(JsValue::NULL)
}
