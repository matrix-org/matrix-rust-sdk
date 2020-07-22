use std::{env, process::exit};

use matrix_sdk::{
    self,
    events::{room::member::MemberEventContent, StrippedStateEvent},
    Client, ClientConfig, EventEmitter, SyncRoom, SyncSettings,
};
use matrix_sdk_common_macros::async_trait;
use url::Url;

struct AutoJoinBot {
    client: Client,
}

impl AutoJoinBot {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl EventEmitter for AutoJoinBot {
    async fn on_stripped_state_member(
        &self,
        room: SyncRoom,
        room_member: &StrippedStateEvent<MemberEventContent>,
        _: Option<MemberEventContent>,
    ) {
        if room_member.state_key != self.client.user_id().await.unwrap() {
            return;
        }

        if let SyncRoom::Invited(room) = room {
            let room = room.read().await;
            println!("Autojoining room {}", room.display_name());
            self.client
                .join_room_by_id(&room.room_id)
                .await
                .expect("Can't join room");
        }
    }
}

async fn login_and_sync(
    homeserver_url: String,
    username: String,
    password: String,
) -> Result<(), matrix_sdk::Error> {
    let mut home = dirs::home_dir().expect("no home directory found");
    home.push("autojoin_bot");

    let client_config = ClientConfig::new().store_path(home);

    let homeserver_url = Url::parse(&homeserver_url).expect("Couldn't parse the homeserver URL");
    let mut client = Client::new_with_config(homeserver_url, client_config).unwrap();

    client
        .login(
            username.clone(),
            password,
            None,
            Some("autojoin bot".to_string()),
        )
        .await?;

    println!("logged in as {}", username);

    client
        .add_event_emitter(Box::new(AutoJoinBot::new(client.clone())))
        .await;

    client
        .sync_forever(SyncSettings::default(), |_| async {})
        .await;

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
