use std::sync::Arc;
use std::{env, process::exit};

use matrix_sdk::{
    self,
    events::room::message::{MessageEvent, MessageEventContent, TextMessageEventContent},
    AsyncClient, AsyncClientConfig, EventEmitter, Room, SyncSettings,
};
use tokio::sync::RwLock;
use url::Url;

struct CommandBot {
    /// This clone of the `AsyncClient` will send requests to the server,
    /// while the other keeps us in sync with the server using `sync_forever`.
    ///
    /// The two type parameters are for the `StateStore` trait and specify the `Store`
    /// type and `IoError` type to use, here we don't care.
    client: AsyncClient<(), ()>,
}

impl CommandBot {
    pub fn new(client: AsyncClient<(), ()>) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl EventEmitter for CommandBot {
    async fn on_room_message(&self, room: Arc<RwLock<Room>>, event: &MessageEvent) {
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
            // we clone here to hold the lock for as little time as possible.
            let room_id = room.read().await.room_id.clone();

            println!("sending");

            self.client
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

    client
        .login(
            username.clone(),
            password,
            None,
            Some("command bot".to_string()),
        )
        .await?;

    println!("logged in as {}", username);

    // initial sync to set up state and so our bot doesn't respond to old messages
    client.sync(SyncSettings::default()).await.unwrap();
    // add our CommandBot to be notified of incoming messages, we do this after the initial
    // sync to avoid responding to messages before the bot was running.
    client
        .add_event_emitter(Box::new(CommandBot::new(client.clone())))
        .await;

    // since we called sync before we `sync_forever` we must pass that sync token to
    // `sync_forever`
    let settings = SyncSettings::default().token(client.sync_token().await.unwrap());
    // this keeps state from the server streaming in to CommandBot via the EventEmitter trait
    client.sync_forever(settings, |_| async {}).await;

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
