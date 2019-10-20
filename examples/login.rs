use std::{env, process::exit};

use matrix_nio::{
    self,
    events::{
        collections::all::RoomEvent,
        room::message::{MessageEvent, MessageEventContent, TextMessageEventContent},
    },
    AsyncClient, AsyncClientConfig, SyncSettings,
};

async fn login(
    homeserver_url: String,
    username: String,
    password: String,
) -> Result<(), matrix_nio::Error> {
    let client_config = AsyncClientConfig::new()
        .proxy("http://localhost:8080")?
        .disable_ssl_verification();
    let mut client = AsyncClient::new_with_config(&homeserver_url, None, client_config).unwrap();

    client.login(username, password, None).await?;
    let response = client.sync(SyncSettings::new()).await?;

    for (room_id, room) in response.rooms.join {
        println!("Room {}", room_id);

        for event in room.timeline.events {
            if let RoomEvent::RoomMessage(MessageEvent {
                content: MessageEventContent::Text(TextMessageEventContent { body: msg_body, .. }),
                sender,
                ..
            }) = event
            {
                println!("{}: {}", sender, msg_body);
            }
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), matrix_nio::Error> {
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
