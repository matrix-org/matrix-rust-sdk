use std::{env, process::exit};

use matrix_sdk::{
    config::SyncSettings,
    room::Room,
    ruma::events::room::message::{
        MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
    },
    Client,
};

async fn on_room_message(event: OriginalSyncRoomMessageEvent, room: Room) {
    if let Room::Joined(room) = room {
        let MessageType::Text(text_content) = event.content.msgtype else {
            return;
        };

        if text_content.body.contains("!party") {
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
    device_id: Option<String>,
) -> anyhow::Result<()> {
    #[allow(unused_mut)]
    let mut client_builder = Client::builder().homeserver_url(homeserver_url);

    // TODO: sled feature is not actually working!
    #[cfg(feature = "sled")]
    {
        // The location to save files to
        let home = dirs::home_dir().expect("no home directory found").join("party_bot");
        client_builder = client_builder.sled_store(home, None)?;
    }

    #[cfg(feature = "redis")]
    {
        println!("Creating a Redis store on 127.0.0.1");
        let redis_url = "redis://127.0.0.1/";
        let redis_prefix = "party_bot";
        client_builder = client_builder.redis_store(redis_url, None, redis_prefix).await?;
    }

    #[cfg(feature = "indexeddb")]
    {
        client_builder = client_builder.indexeddb_store("party_bot", None).await?;
    }

    let client = client_builder.build().await.unwrap();

    let mut login = client.login_username(&username, &password);
    if let Some(device_id) = &device_id {
        login = login.device_id(device_id);
    }
    login.initial_device_display_name("command bot").await?;

    println!("logged in as {username}");

    // An initial sync to set up state and so our bot doesn't respond to old
    // messages. If the `StateStore` finds saved state in the location given the
    // initial sync will be skipped in favor of loading state from the store
    let response = client.sync_once(SyncSettings::default()).await.unwrap();
    // add our CommandBot to be notified of incoming messages, we do this after the
    // initial sync to avoid responding to messages before the bot was running.
    client.add_event_handler(on_room_message);

    // since we called `sync_once` before we entered our sync loop we must pass
    // that sync token to `sync`
    let settings = SyncSettings::default().token(response.next_batch);
    // this keeps state from the server streaming in to CommandBot via the
    // EventHandler trait
    client.sync(settings).await?;

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let (homeserver_url, username, password) =
        match (env::args().nth(1), env::args().nth(2), env::args().nth(3)) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => {
                eprintln!(
                    "Usage: {} \
                        <homeserver_url> \
                        <username> \
                        <password> \
                        [<device_id>]",
                    env::args().next().unwrap()
                );
                exit(1)
            }
        };

    login_and_sync(homeserver_url, username, password, env::args().nth(4)).await?;
    Ok(())
}
