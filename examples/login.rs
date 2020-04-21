use std::sync::Arc;
use std::{env, process::exit};
use url::Url;

use matrix_sdk::{
    self,
    events::room::message::{MessageEvent, MessageEventContent, TextMessageEventContent},
    AsyncClient, AsyncClientConfig, EventEmitter, Room, SyncSettings,
};
use tokio::sync::RwLock;

struct EventCallback;

#[async_trait::async_trait]
impl EventEmitter for EventCallback {
    async fn on_room_message(&self, room: Arc<RwLock<Room>>, event: &MessageEvent) {
        if let MessageEvent {
            content: MessageEventContent::Text(TextMessageEventContent { body: msg_body, .. }),
            sender,
            ..
        } = event
        {
            let name = {
                // any reads should be held for the shortest time possible to
                // avoid dead locks
                let room = room.read().await;
                let member = room.members.get(&sender).unwrap();
                member
                    .display_name
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or(sender.to_string())
            };
            println!("{}: {}", name, msg_body);
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
    let mut client =
        AsyncClient::<()>::new_with_config(homeserver_url, None, client_config).unwrap();

    client.add_event_emitter(Box::new(EventCallback)).await;

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
