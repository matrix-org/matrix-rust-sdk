use std::ops::Deref;
use std::sync::Arc;
use std::{env, process::exit};
use url::Url;

use matrix_sdk::{
    self,
    events::room::message::{MessageEvent, MessageEventContent, TextMessageEventContent},
    identifiers::RoomId,
    AsyncClient, AsyncClientConfig, EventEmitter, Room, SyncSettings,
};
use tokio::runtime::Handle;
use tokio::sync::{
    mpsc::{self, Sender},
    Mutex,
};

struct CommandBot {
    send: Sender<(RoomId, String)>,
}

impl CommandBot {
    pub fn new(send: Sender<(RoomId, String)>) -> Self {
        Self { send }
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
                self.send
                    .send((
                        room.room_id.clone(),
                        "🎉🎊🥳 let's PARTY!! 🥳🎊🎉".to_string(),
                    ))
                    .await
                    .unwrap()
            }
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

    let (send, mut recv) = mpsc::channel(100);

    client
        .add_event_emitter(Arc::new(Mutex::new(Box::new(CommandBot::new(send)))))
        .await;

    client
        .login(
            username.clone(),
            password,
            None,
            Some("command bot".to_string()),
        )
        .await?;

    println!("logged in as user {}", username);

    let client = Arc::new(Mutex::new(client));
    let send_client = Arc::clone(&client);

    exec.spawn(async move {
        for (id, msg) in recv.recv().await {
            let content = MessageEventContent::Text(TextMessageEventContent {
                body: msg,
                format: None,
                formatted_body: None,
                relates_to: None,
            });
            send_client
                .lock()
                .await
                .room_send(&id, content, None)
                .await
                .unwrap();
        }
    });

    client
        .lock()
        .await
        .sync_forever(SyncSettings::new(), |_| async {})
        .await;

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

    let executor = runtime.handle().clone();
    runtime.block_on(async { login_and_sync(homeserver_url, username, password, executor).await })
}
