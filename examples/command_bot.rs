use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, process::exit};

use js_int::UInt;
use matrix_sdk::{
    self,
    events::room::message::{MessageEvent, MessageEventContent, TextMessageEventContent},
    AsyncClient, AsyncClientConfig, EventEmitter, Room, SyncSettings,
};
use tokio::sync::{Mutex, RwLock};
use url::Url;

struct CommandBot {
    /// This clone of the `AsyncClient` will send requests to the server,
    /// while the other keeps us in sync with the server using `sync_forever`.
    client: Mutex<AsyncClient>,
    /// A timestamp so we only respond to messages sent after the bot is running.
    start_time: UInt,
}

impl CommandBot {
    pub fn new(client: AsyncClient) -> Self {
        let now = SystemTime::now();
        let timestamp = now.duration_since(UNIX_EPOCH).unwrap().as_millis();
        Self {
            client: Mutex::new(client),
            start_time: UInt::new(timestamp as u64).unwrap(),
        }
    }
}

#[async_trait::async_trait]
impl EventEmitter for CommandBot {
    async fn on_room_message(&self, room: Arc<RwLock<Room>>, event: &MessageEvent) {
        let (msg_body, timestamp) = if let MessageEvent {
            content: MessageEventContent::Text(TextMessageEventContent { body: msg_body, .. }),
            origin_server_ts,
            ..
        } = event
        {
            (msg_body.clone(), *origin_server_ts)
        } else {
            (String::new(), UInt::min_value())
        };

        if msg_body.contains("!party") && timestamp > self.start_time {
            let content = MessageEventContent::Text(TextMessageEventContent {
                body: "🎉🎊🥳 let's PARTY!! 🥳🎊🎉".to_string(),
                format: None,
                formatted_body: None,
                relates_to: None,
            });
            // we clone here to hold the lock for as little time as possible.
            let room_id = room.read().await.room_id.clone();

            println!("sending");

            self.client
                .lock()
                .await
                // send our message to the room we found the "!party" command in
                // the last parameter is an optional Uuid which we don't care about.
                .room_send(&room_id, content, None)
                .await
                .unwrap();

            println!("message sent");
        }
    }
}

async fn login_and_sync(
    homeserver_url: String,
    username: String,
    password: String,
) -> Result<(), matrix_sdk::Error> {
    let client_config = AsyncClientConfig::new()
        .proxy("http://localhost:8080")?
        .disable_ssl_verification();

    let homeserver_url = Url::parse(&homeserver_url)?;
    // create a new AsyncClient with the given homeserver url and config
    let mut client = AsyncClient::new_with_config(homeserver_url, None, client_config).unwrap();
    // add our CommandBot to be notified of incoming messages
    client
        .add_event_emitter(Box::new(CommandBot::new(client.clone())))
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

    // this keeps state from the server streaming in to CommandBot via the EventEmitter trait
    client.sync_forever(SyncSettings::new(), |_| async {}).await;

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
