use std::{env, process::exit};

use matrix_sdk::{
    config::SyncSettings, room::Room, ruma::events::room::member::StrippedRoomMemberEvent, Client,
};
use tokio::time::{sleep, Duration};

async fn on_stripped_state_member(
    room_member: StrippedRoomMemberEvent,
    client: Client,
    room: Room,
) {
    if room_member.state_key != client.user_id().unwrap() {
        return;
    }

    if let Room::Invited(room) = room {
        tokio::spawn(async move {
            println!("Autojoining room {}", room.room_id());
            let mut delay = 2;

            while let Err(err) = room.accept_invitation().await {
                // retry autojoin due to synapse sending invites, before the
                // invited user can join for more information see
                // https://github.com/matrix-org/synapse/issues/4345
                eprintln!("Failed to join room {} ({err:?}), retrying in {delay}s", room.room_id());

                sleep(Duration::from_secs(delay)).await;
                delay *= 2;

                if delay > 3600 {
                    eprintln!("Can't join room {} ({err:?})", room.room_id());
                    break;
                }
            }
            println!("Successfully joined room {}", room.room_id());
        });
    }
}

async fn login_and_sync(
    homeserver_url: String,
    username: &str,
    password: &str,
) -> anyhow::Result<()> {
    #[allow(unused_mut)]
    let mut client_builder = Client::builder().homeserver_url(homeserver_url);

    #[cfg(feature = "sled")]
    {
        // The location to save files to
        let home = dirs::home_dir().expect("no home directory found").join("autojoin_bot");
        client_builder = client_builder.sled_store(home, None)?;
    }

    #[cfg(feature = "indexeddb")]
    {
        client_builder = client_builder.indexeddb_store("autojoin_bot", None).await?;
    }

    let client = client_builder.build().await?;

    client
        .login_username(username, password)
        .initial_device_display_name("autojoin bot")
        .send()
        .await?;

    println!("logged in as {username}");

    client.add_event_handler(on_stripped_state_member);

    client.sync(SyncSettings::default()).await;

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
                    "Usage: {} <homeserver_url> <username> <password>",
                    env::args().next().unwrap()
                );
                exit(1)
            }
        };

    login_and_sync(homeserver_url, &username, &password).await?;
    Ok(())
}
