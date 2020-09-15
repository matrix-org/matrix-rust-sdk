use std::{
    env,
    fs::File,
    io::{Seek, SeekFrom},
    path::PathBuf,
    process::exit,
    sync::Arc,
};
use tokio::sync::Mutex;

use matrix_sdk::{
    self,
    events::{
        room::message::{MessageEventContent, TextMessageEventContent},
        SyncMessageEvent,
    },
    Client, ClientConfig, EventEmitter, SyncRoom, SyncSettings,
};
use matrix_sdk_common_macros::async_trait;
use url::Url;

struct ImageBot {
    client: Client,
    image: Arc<Mutex<File>>,
}

impl ImageBot {
    pub fn new(client: Client, image: File) -> Self {
        let image = Arc::new(Mutex::new(image));
        Self { client, image }
    }
}

#[async_trait]
impl EventEmitter for ImageBot {
    async fn on_room_message(&self, room: SyncRoom, event: &SyncMessageEvent<MessageEventContent>) {
        if let SyncRoom::Joined(room) = room {
            let msg_body = if let SyncMessageEvent {
                content: MessageEventContent::Text(TextMessageEventContent { body: msg_body, .. }),
                ..
            } = event
            {
                msg_body
            } else {
                return;
            };

            if msg_body.contains("!image") {
                let room_id = room.read().await.room_id.clone();

                println!("sending image");
                let mut image = self.image.lock().await;

                self.client
                    .room_send_attachment(&room_id, "cat", "image/jpg", &mut *image, None)
                    .await
                    .unwrap();

                image.seek(SeekFrom::Start(0)).unwrap();

                println!("message sent");
            }
        }
    }
}

async fn login_and_sync(
    homeserver_url: String,
    username: String,
    password: String,
    image: File,
) -> Result<(), matrix_sdk::Error> {
    let client_config = ClientConfig::new()
        .proxy("http://localhost:8080")?
        .disable_ssl_verification();

    let homeserver_url = Url::parse(&homeserver_url).expect("Couldn't parse the homeserver URL");
    let mut client = Client::new_with_config(homeserver_url, client_config).unwrap();

    client
        .login(&username, &password, None, Some("command bot"))
        .await?;

    client.sync(SyncSettings::default()).await.unwrap();
    client
        .add_event_emitter(Box::new(ImageBot::new(client.clone(), image)))
        .await;

    let settings = SyncSettings::default().token(client.sync_token().await.unwrap());
    client.sync_forever(settings, |_| async {}).await;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), matrix_sdk::Error> {
    tracing_subscriber::fmt::init();
    let (homeserver_url, username, password, image_path) = match (
        env::args().nth(1),
        env::args().nth(2),
        env::args().nth(3),
        env::args().nth(4),
    ) {
        (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
        _ => {
            eprintln!(
                "Usage: {} <homeserver_url> <username> <password> <image>",
                env::args().next().unwrap()
            );
            exit(1)
        }
    };

    println!(
        "helloooo {} {} {} {:#?}",
        homeserver_url, username, password, image_path
    );
    let path = PathBuf::from(image_path);
    let image = File::open(path).expect("Can't open image file.");

    login_and_sync(homeserver_url, username, password, image).await?;
    Ok(())
}
