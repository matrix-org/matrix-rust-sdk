use std::sync::{Arc, Mutex};
use std::{env, process::exit};
use url::Url;

use matrix_sdk::{
    self,
    events::{
        collections::all::RoomEvent,
        room::message::{MessageEvent, MessageEventContent, TextMessageEventContent},
    },
    AsyncClient, AsyncClientConfig, EventEmitter, Room, SyncSettings,
};

struct EventCallback;

#[async_trait::async_trait]
impl EventEmitter for EventCallback {
    async fn on_room_message(&mut self, room: &Room, event: &RoomEvent) {
        if let RoomEvent::RoomMessage(MessageEvent {
            content: MessageEventContent::Text(TextMessageEventContent { body: msg_body, .. }),
            sender,
            ..
        }) = event
        {
            let member = room.members.get(&sender.to_string()).unwrap();
            println!(
                "{}: {}",
                member
                    .user
                    .display_name
                    .as_ref()
                    .unwrap_or(&sender.to_string()),
                msg_body
            );
        }
    }
}

async fn login(
    homeserver_url: String,
    username: String,
    password: String,
) -> Result<(), matrix_sdk::Error> {
    let client_config = AsyncClientConfig::new()
        .proxy("http://localhost:8080")?
        .disable_ssl_verification();
    let homeserver_url = Url::parse(&homeserver_url)?;
    let mut client = AsyncClient::new_with_config(homeserver_url, None, client_config).unwrap();

    client
        .add_event_emitter(Arc::new(Mutex::new(EventCallback)))
        .await;

    client
        .login(username, password, None, Some("rust-sdk".to_string()))
        .await?;
    client.sync_forever(SyncSettings::new(), |_| async {}).await;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), matrix_sdk::Error> {
    tracing_subscriber::fmt::init();

    let (homeserver_url, username, password) =
        match (env::args().nth(1), env::args().nth(2), env::args().nth(3)) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => {
                eprintln!(
                    "Usage: {} <homeserver_url> <username> <password>",
                    env::args().next().unwrap()
                );
                exit(1)
            }
        };

    login(homeserver_url, username, password).await
}
