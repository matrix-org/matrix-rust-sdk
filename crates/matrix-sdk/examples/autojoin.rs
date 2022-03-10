use std::{env, process::exit};

use matrix_sdk::{
    config::{ClientConfig, SyncSettings},
    room::Room,
    ruma::events::room::member::StrippedRoomMemberEvent,
    Client,
};
use tokio::time::{sleep, Duration};
use url::Url;

async fn on_stripped_state_member(
    room_member: StrippedRoomMemberEvent,
    client: Client,
    room: Room,
) {
    if room_member.state_key != client.user_id().await.unwrap() {
        return;
    }

    if let Room::Invited(room) = room {
        println!("Autojoining room {}", room.room_id());
        let mut delay = 2;

        while let Err(err) = room.accept_invitation().await {
            // retry autojoin due to synapse sending invites, before the
            // invited user can join for more information see
            // https://github.com/matrix-org/synapse/issues/4345
            eprintln!("Failed to join room {} ({:?}), retrying in {}s", room.room_id(), err, delay);

            sleep(Duration::from_secs(delay)).await;
            delay *= 2;

            if delay > 3600 {
                eprintln!("Can't join room {} ({:?})", room.room_id(), err);
                break;
            }
        }
        println!("Successfully joined room {}", room.room_id());
    }
}

async fn login_and_sync(
    homeserver_url: String,
    username: &str,
    password: &str,
) -> Result<(), matrix_sdk::Error> {
    #[cfg(not(any(feature = "sled_state_store", feature = "indexeddb_state_store")))]
    let client_config = ClientConfig::new();
    #[cfg(any(feature = "sled_state_store", feature = "indexeddb_state_store"))]
    let mut client_config = ClientConfig::new();

    #[cfg(feature = "sled_state_store")]
    {
        // The location to save files to
        let mut home = dirs::home_dir().expect("no home directory found");
        home.push("autojoin_bot");
        let state_store = matrix_sdk_sled::StateStore::open_with_path(home)?;
        client_config = client_config.state_store(Box::new(state_store));
    }

    #[cfg(feature = "indexeddb_state_store")]
    {
        let state_store = matrix_sdk_indexeddb::StateStore::open();
        client_config = client_config.state_store(Box::new(state_store));
    }

    let homeserver_url = Url::parse(&homeserver_url).expect("Couldn't parse the homeserver URL");
    let client = Client::new_with_config(homeserver_url, client_config).await.unwrap();

    client.login(username, password, None, Some("autojoin bot")).await?;

    println!("logged in as {}", username);

    client.register_event_handler(on_stripped_state_member).await;

    client.sync(SyncSettings::default()).await;

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

    login_and_sync(homeserver_url, &username, &password).await?;
    Ok(())
}
