use matrix_sdk::{
    deserialized_responses::SyncResponse,
    events::{
        room::message::{MessageEventContent, MessageType, TextMessageEventContent},
        AnyMessageEventContent, AnySyncMessageEvent, AnySyncRoomEvent, SyncMessageEvent,
    },
    identifiers::RoomId,
    Client, LoopCtrl, SyncSettings,
};
use url::Url;
use wasm_bindgen::prelude::*;
use web_sys::console;

struct WasmBot(Client);

impl WasmBot {
    async fn on_room_message(
        &self,
        room_id: &RoomId,
        event: &SyncMessageEvent<MessageEventContent>,
    ) {
        let msg_body = if let SyncMessageEvent {
            content:
                MessageEventContent {
                    msgtype: MessageType::Text(TextMessageEventContent { body: msg_body, .. }),
                    ..
                },
            ..
        } = event
        {
            msg_body
        } else {
            return;
        };

        console::log_1(&format!("Received message event {:?}", &msg_body).into());

        if msg_body.contains("!party") {
            let content = AnyMessageEventContent::RoomMessage(MessageEventContent::text_plain(
                "🎉🎊🥳 let's PARTY!! 🥳🎊🎉",
            ));

            println!("sending");

            self.0
                // send our message to the room we found the "!party" command in
                // the last parameter is an optional Uuid which we don't care about.
                .room_send(room_id, content, None)
                .await
                .unwrap();

            println!("message sent");
        }
    }

    async fn on_sync_response(&self, response: SyncResponse) -> LoopCtrl {
        console::log_1(&"Synced".to_string().into());

        for (room_id, room) in response.rooms.join {
            for event in room.timeline.events {
                if let Ok(AnySyncRoomEvent::Message(AnySyncMessageEvent::RoomMessage(ev))) = event.event.deserialize() {
                    self.on_room_message(&room_id, &ev).await
                }
            }
        }

        LoopCtrl::Continue
    }
}

#[wasm_bindgen]
pub async fn run() -> Result<JsValue, JsValue> {
    console_error_panic_hook::set_once();

    let homeserver_url = "http://localhost:8008";
    let username = "example";
    let password = "wordpass";

    let homeserver_url = Url::parse(&homeserver_url).unwrap();
    let client = Client::new(homeserver_url).unwrap();

    client
        .login(username, password, None, Some("rust-sdk-wasm"))
        .await
        .unwrap();

    let bot = WasmBot(client.clone());

    client.sync_once(SyncSettings::default()).await.unwrap();

    let settings = SyncSettings::default().token(client.sync_token().await.unwrap());
    client
        .sync_with_callback(settings, |response| bot.on_sync_response(response))
        .await;

    Ok(JsValue::NULL)
}
