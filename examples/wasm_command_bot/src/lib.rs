use matrix_sdk::{
    config::SyncSettings,
    ruma::{
        events::{
            room::message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent},
            AnyMessageLikeEventContent, AnySyncMessageLikeEvent, AnySyncTimelineEvent,
            SyncMessageLikeEvent,
        },
        RoomId,
    },
    sync::SyncResponse,
    Client, LoopCtrl,
};
use url::Url;
use wasm_bindgen::prelude::*;
use web_sys::console;

struct WasmBot(Client);

impl WasmBot {
    async fn on_room_message(&self, room_id: &RoomId, event: &OriginalSyncRoomMessageEvent) {
        let MessageType::Text(text_content) = &event.content.msgtype else { return };

        console::log_1(&format!("Received message event {:?}", &text_content.body).into());

        if text_content.body.contains("!party") {
            let content = AnyMessageLikeEventContent::RoomMessage(
                RoomMessageEventContent::text_plain("🎉🎊🥳 let's PARTY!! 🥳🎊🎉"),
            );

            println!("sending");

            if let Some(room) = self.0.get_joined_room(room_id) {
                // send our message to the room we found the "!party" command in
                // the last parameter is an optional transaction id which we
                // don't care about.
                room.send(content, None).await.unwrap();
            }

            println!("message sent");
        }
    }

    async fn on_sync_response(&self, response: SyncResponse) -> LoopCtrl {
        console::log_1(&"Synced".to_owned().into());

        for (room_id, room) in response.rooms.join {
            for event in room.timeline.events {
                if let Ok(AnySyncTimelineEvent::MessageLike(
                    AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(ev)),
                )) = event.event.deserialize()
                {
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

    let homeserver_url = Url::parse(homeserver_url).unwrap();
    let client = Client::new(homeserver_url).await.unwrap();

    client
        .matrix_auth()
        .login_username(username, password)
        .initial_device_display_name("rust-sdk-wasm")
        .await
        .unwrap();

    let bot = WasmBot(client.clone());

    let response = client.sync_once(SyncSettings::default()).await.unwrap();

    let settings = SyncSettings::default().token(response.next_batch);
    client.sync_with_callback(settings, |response| bot.on_sync_response(response)).await.unwrap();

    Ok(JsValue::NULL)
}
