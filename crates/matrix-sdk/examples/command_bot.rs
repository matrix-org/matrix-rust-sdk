use std::{env, process::exit};

use matrix_sdk::{
    config::{ClientConfig, SyncSettings},
    room::Room,
    ruma::events::room::message::{
        MessageType, RoomMessageEventContent, SyncRoomMessageEvent, TextMessageEventContent,
    },
    Client,
};
use url::Url;

async fn on_room_message(event: SyncRoomMessageEvent, room: Room) {
    if let Room::Joined(room) = room {
        let msg_body = match event.content.msgtype {
            MessageType::Text(TextMessageEventContent { body, .. }) => body,
            _ => return,
        };

        if msg_body.contains("!party") {
            let content = RoomMessageEventContent::text_plain("🎉🎊🥳 let's PARTY!! 🥳🎊🎉");

            println!("sending");

            // send our message to the room we found the "!party" command in
            // the last parameter is an optional transaction id which we don't
            // care about.
            room.send(content, None).await.unwrap();

            println!("message sent");
        }
    }
}

async fn login_and_sync(
    homeserver_url: String,
    username: String,
    password: String,
) -> Result<(), matrix_sdk::Error> {
    #[cfg(not(any(feature = "sled_state_store", feature = "indexeddb_stores")))]
    let client_config = ClientConfig::new();
    #[cfg(any(feature = "sled_state_store", feature = "indexeddb_stores"))]
    let mut client_config = ClientConfig::new();

    #[cfg(feature = "sled_state_store")]
    {
        // The location to save files to
        let mut home = dirs::home_dir().expect("no home directory found");
        home.push("party_bot");
        let state_store = matrix_sdk_sled::StateStore::open_with_path(home)?;
        client_config = client_config.state_store(Box::new(state_store));
    }

    #[cfg(feature = "indexeddb_stores")]
    {
        let state_store = matrix_sdk_indexeddb::StateStore::open();
        client_config = client_config.state_store(Box::new(state_store));
    }

    let homeserver_url = Url::parse(&homeserver_url).expect("Couldn't parse the homeserver URL");
    // create a new Client with the given homeserver url and config
    let client = Client::new_with_config(homeserver_url, client_config).await.unwrap();

    client.login(&username, &password, None, Some("command bot")).await?;

    println!("logged in as {}", username);

    // An initial sync to set up state and so our bot doesn't respond to old
    // messages. If the `StateStore` finds saved state in the location given the
    // initial sync will be skipped in favor of loading state from the store
    client.sync_once(SyncSettings::default()).await.unwrap();
    // add our CommandBot to be notified of incoming messages, we do this after the
    // initial sync to avoid responding to messages before the bot was running.
    client.register_event_handler(on_room_message).await;

    // since we called `sync_once` before we entered our sync loop we must pass
    // that sync token to `sync`
    let settings = SyncSettings::default().token(client.sync_token().await.unwrap());
    // this keeps state from the server streaming in to CommandBot via the
    // EventHandler trait
    client.sync(settings).await;

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

    login_and_sync(homeserver_url, username, password).await?;
    Ok(())
}
