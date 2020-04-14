use std::ops::Deref;
use std::sync::Arc;
use std::{env, process::exit};
use url::Url;

use matrix_sdk::{
    self,
    events::room::message::{MessageEvent, MessageEventContent, TextMessageEventContent},
    AsyncClient, AsyncClientConfig, EventEmitter, Room, SyncSettings,
};
use tokio::sync::Mutex;

struct CommandBot {
    client: AsyncClient,
}

impl CommandBot {
    pub fn new(client: AsyncClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl EventEmitter for CommandBot {
    async fn on_room_message(&mut self, room: Arc<Mutex<Room>>, event: Arc<Mutex<MessageEvent>>) {
        if let MessageEvent {
            content: MessageEventContent::Text(TextMessageEventContent { body: msg_body, .. }),
            ..
        } = event.lock().await.deref()
        {
            let room = room.lock().await;
            if msg_body.contains("!party") {
                println!("!party found");
                let content = MessageEventContent::Text(TextMessageEventContent {
                    body: "🎉🎊🥳 let's PARTY!! 🥳🎊🎉".to_string(),
                    format: None,
                    formatted_body: None,
                    relates_to: None,
                });
                self.client
                    .room_send(&room.room_id, content, None)
                    .await
                    .unwrap();
                println!("message sent");
            }
        }
    }
}

#[allow(clippy::for_loop_over_option)]
async fn login_and_sync(
    homeserver_url: String,
    username: String,
    password: String,
) -> Result<(), matrix_sdk::Error> {
    let client_config = AsyncClientConfig::new();
    // .proxy("http://localhost:8080")?
    // .disable_ssl_verification();
    let homeserver_url = Url::parse(&homeserver_url)?;
    let mut client = AsyncClient::new_with_config(homeserver_url, None, client_config).unwrap();

    client
        .add_event_emitter(Arc::new(Mutex::new(Box::new(CommandBot::new(
            client.clone(),
        )))))
        .await;

    client
        .login(
            username.clone(),
            password,
            None,
            Some("command bot".to_string()),
        )
        .await?;

    println!("logged in as {}", username);

    client.sync(SyncSettings::new()).await.unwrap();

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), matrix_sdk::Error> {
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
    login_and_sync(homeserver_url, username, password).await?;
    Ok(())
}
