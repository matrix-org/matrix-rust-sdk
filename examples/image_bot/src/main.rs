use std::{env, fs, process::exit, sync::Arc};

use matrix_sdk::{
    self,
    attachment::AttachmentConfig,
    config::SyncSettings,
    room::Room,
    ruma::events::room::message::{
        MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent, TextMessageEventContent,
    },
    Client,
};
use url::Url;

async fn on_room_message(event: OriginalSyncRoomMessageEvent, room: Room, image: Arc<[u8]>) {
    if let Room::Joined(room) = room {
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

        if msg_body.contains("!image") {
            println!("sending image");
            room.send_attachment("cat", &mime::IMAGE_JPEG, &image, AttachmentConfig::new())
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
    image: Arc<[u8]>,
) -> matrix_sdk::Result<()> {
    let homeserver_url = Url::parse(&homeserver_url).expect("Couldn't parse the homeserver URL");
    let client = Client::new(homeserver_url).await.unwrap();

    client
        .login_username(&username, &password)
        .initial_device_display_name("command bot")
        .send()
        .await?;

    client.sync_once(SyncSettings::default()).await.unwrap();

    client.add_event_handler(move |ev, room| on_room_message(ev, room, image.clone()));

    let settings = SyncSettings::default().token(client.sync_token().await.unwrap());
    client.sync(settings).await;

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let (homeserver_url, username, password, image_path) =
        match (env::args().nth(1), env::args().nth(2), env::args().nth(3), env::args().nth(4)) {
            (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
            _ => {
                eprintln!(
                    "Usage: {} <homeserver_url> <username> <password> <image>",
                    env::args().next().unwrap()
                );
                exit(1)
            }
        };

    println!("helloooo {homeserver_url} {username} {password} {image_path:#?}");
    let image = fs::read(&image_path).expect("Can't open image file.").into();

    login_and_sync(homeserver_url, username, password, image).await?;
    Ok(())
}
