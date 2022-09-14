use matrix_sdk::{
    config::SyncSettings,
    deserialized_responses::SyncResponse,
    ruma::{
        events::{
            room::message::{
                MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
                TextMessageEventContent,
            },
            AnyMessageLikeEventContent, AnySyncMessageLikeEvent, AnySyncTimelineEvent,
            SyncMessageLikeEvent,
        },
        RoomId,
    },
    Client, LoopCtrl,
};
use url::Url;
use wasm_bindgen::prelude::*;
use web_sys::console;

struct WasmBot(Client);

impl WasmBot {
    async fn on_room_message(&self, room_id: &RoomId, event: &OriginalSyncRoomMessageEvent) {
        let msg_body = if let OriginalSyncRoomMessageEvent {
            content:
                RoomMessageEventContent {
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
        .login_username(username, password)
        .initial_device_display_name("rust-sdk-wasm")
        .send()
        .await
        .unwrap();

    let bot = WasmBot(client.clone());

    client.sync_once(SyncSettings::default()).await.unwrap();

    let settings = SyncSettings::default().token(client.sync_token().await.unwrap());
    client.sync_with_callback(settings, |response| bot.on_sync_response(response)).await;

    Ok(JsValue::NULL)
}
