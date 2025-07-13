use std::{env, process::exit};

use matrix_sdk::{
    Client, Room, config::SyncSettings, ruma::events::room::member::StrippedRoomMemberEvent,
};
use tokio::time::{Duration, sleep};

async fn on_stripped_state_member(
    room_member: StrippedRoomMemberEvent,
    client: Client,
    room: Room,
) {
    if room_member.state_key != client.user_id().unwrap() {
        return;
    }

    tokio::spawn(async move {
        println!("Autojoining room {}", room.room_id());
        let mut delay = 2;

        while let Err(err) = room.join().await {
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

async fn login_and_sync(
    homeserver_url: String,
    username: &str,
    password: &str,
) -> anyhow::Result<()> {
    // Note that when encryption is enabled, you should use a persistent store to be
    // able to restore the session with a working encryption setup.
    // See the `persist_session` example.
    let client = Client::builder().homeserver_url(homeserver_url).build().await?;

    client
        .matrix_auth()
        .login_username(username, password)
        .initial_device_display_name("autojoin bot")
        .await?;

    println!("logged in as {username}");

    client.add_event_handler(on_stripped_state_member);

    client.sync(SyncSettings::default()).await?;

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // parse the command line for homeserver, username and password
    let (Some(homeserver_url), Some(username), Some(password)) =
        (env::args().nth(1), env::args().nth(2), env::args().nth(3))
    else {
        eprintln!("Usage: {} <homeserver_url> <username> <password>", env::args().next().unwrap());
        exit(1)
    };

    login_and_sync(homeserver_url, &username, &password).await?;
    Ok(())
}
