use std::ops::Deref;
use std::sync::Arc;
use std::{env, process::exit};
use url::Url;

use matrix_sdk::{
    self,
    events::room::message::{MessageEvent, MessageEventContent, TextMessageEventContent},
    AsyncClient, AsyncClientConfig, EventEmitter, Room, SyncSettings,
};
use tokio::runtime::{Handle, Runtime};
use tokio::sync::mpsc::{channel, Sender};
use tokio::sync::Mutex;

struct CommandBot {
    client: Mutex<AsyncClient>,
    // sender: Sender<(RoomId, MessageEventContent)>
}

impl CommandBot {
    pub fn new(client: AsyncClient) -> Self {
        Self {
            client: Mutex::new(client),
        }
    }
}

#[async_trait::async_trait]
impl EventEmitter for CommandBot {
    async fn on_room_message(&self, room: &Room, event: &MessageEvent) {
        let msg_body = if let MessageEvent {
            content: MessageEventContent::Text(TextMessageEventContent { body: msg_body, .. }),
            ..
        } = event
        {
            msg_body.clone()
        } else {
            String::new()
        };

        if msg_body.contains("!party") {
            let content = MessageEventContent::Text(TextMessageEventContent {
                body: "🎉🎊🥳 let's PARTY!! 🥳🎊🎉".to_string(),
                format: None,
                formatted_body: None,
                relates_to: None,
            });
            let room_id = &room.room_id;

            println!("sending");

            self.client
                .lock()
                .await
                .room_send(&room_id, content, None)
                .await
                .unwrap();

            println!("message sent");
        }
    }
}

#[allow(clippy::for_loop_over_option)]
async fn login_and_sync(
    homeserver_url: String,
    username: String,
    password: String,
    exec: Handle,
) -> Result<(), matrix_sdk::Error> {
    let client_config = AsyncClientConfig::new();
    // .proxy("http://localhost:8080")?
    // .disable_ssl_verification();
    let homeserver_url = Url::parse(&homeserver_url)?;
    let mut client = AsyncClient::new_with_config(homeserver_url, None, client_config).unwrap();

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

    exec.spawn(async move {
        client.sync_forever(SyncSettings::new(), |_| async {}).await;
    })
    .await
    .unwrap();

    Ok(())
}

fn main() -> Result<(), matrix_sdk::Error> {
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

    let mut runtime = tokio::runtime::Builder::new()
        .basic_scheduler()
        .threaded_scheduler()
        .enable_all()
        .build()
        .unwrap();

    let exec = runtime.handle().clone();

    runtime.block_on(async { login_and_sync(homeserver_url, username, password, exec).await })?;
    Ok(())
}
