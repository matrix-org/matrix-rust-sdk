use std::{
    env, fmt,
    io::{self, Write},
    process::exit,
};

use anyhow::anyhow;
use matrix_sdk::{
    Client, Room, RoomState,
    config::SyncSettings,
    ruma::{
        api::client::session::get_login_types::v3::{IdentityProvider, LoginType},
        events::room::message::{MessageType, OriginalSyncRoomMessageEvent},
    },
};
use url::Url;

/// The initial device name when logging in with a device for the first time.
const INITIAL_DEVICE_DISPLAY_NAME: &str = "login client";

/// A simple program that adapts to the different login methods offered by a
/// Matrix homeserver.
///
/// Homeservers usually offer to login either via password, Single Sign-On (SSO)
/// or both.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let Some(homeserver_url) = env::args().nth(1) else {
        eprintln!("Usage: {} <homeserver_url>", env::args().next().unwrap());
        exit(1)
    };

    login_and_sync(homeserver_url).await?;

    Ok(())
}

/// Log in to the given homeserver and sync.
async fn login_and_sync(homeserver_url: String) -> anyhow::Result<()> {
    let homeserver_url = Url::parse(&homeserver_url)?;
    let client = Client::new(homeserver_url).await?;

    // First, let's figure out what login types are supported by the homeserver.
    let mut choices = Vec::new();
    let login_types = client.matrix_auth().get_login_types().await?.flows;

    for login_type in login_types {
        match login_type {
            LoginType::Password(_) => {
                choices.push(LoginChoice::Password)
            }
            LoginType::Sso(sso) => {
                if sso.identity_providers.is_empty() {
                    choices.push(LoginChoice::Sso)
                } else {
                    choices.extend(sso.identity_providers.into_iter().map(LoginChoice::SsoIdp))
                }
            }
            // This is used for SSO, so it's not a separate choice.
            LoginType::Token(_) |
            // This is only for application services, ignore it here.
            LoginType::ApplicationService(_) => {},
            // We don't support unknown login types.
            _ => {},
        }
    }

    match choices.len() {
        0 => return Err(anyhow!("Homeserver login types incompatible with this client")),
        1 => choices[0].login(&client).await?,
        _ => offer_choices_and_login(&client, choices).await?,
    }

    // Now that we are logged in, we can sync and listen to new messages.
    client.add_event_handler(on_room_message);
    // This will sync until an error happens or the program is killed.
    client.sync(SyncSettings::new()).await?;

    Ok(())
}

#[derive(Debug)]
enum LoginChoice {
    /// Login with username and password.
    Password,

    /// Login with SSO.
    Sso,

    /// Login with a specific SSO identity provider.
    SsoIdp(IdentityProvider),
}

impl LoginChoice {
    /// Login with this login choice.
    async fn login(&self, client: &Client) -> anyhow::Result<()> {
        match self {
            LoginChoice::Password => login_with_password(client).await,
            LoginChoice::Sso => login_with_sso(client, None).await,
            LoginChoice::SsoIdp(idp) => login_with_sso(client, Some(idp)).await,
        }
    }
}

impl fmt::Display for LoginChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoginChoice::Password => write!(f, "Username and password"),
            LoginChoice::Sso => write!(f, "SSO"),
            LoginChoice::SsoIdp(idp) => write!(f, "SSO via {}", idp.name),
        }
    }
}

/// Offer the given choices to the user and login with the selected option.
async fn offer_choices_and_login(client: &Client, choices: Vec<LoginChoice>) -> anyhow::Result<()> {
    println!("Several options are available to login with this homeserver:\n");

    let choice = loop {
        for (idx, login_choice) in choices.iter().enumerate() {
            println!("{idx}) {login_choice}");
        }

        print!("\nEnter your choice: ");
        io::stdout().flush().expect("Unable to write to stdout");
        let mut choice_str = String::new();
        io::stdin().read_line(&mut choice_str).expect("Unable to read user input");

        match choice_str.trim().parse::<usize>() {
            Ok(choice) => {
                if choice >= choices.len() {
                    eprintln!("This is not a valid choice");
                } else {
                    break choice;
                }
            }
            Err(_) => eprintln!("This is not a valid choice. Try again.\n"),
        }
    };

    choices[choice].login(client).await?;

    Ok(())
}

/// Login with a username and password.
async fn login_with_password(client: &Client) -> anyhow::Result<()> {
    println!("Logging in with username and password…");

    loop {
        print!("\nUsername: ");
        io::stdout().flush().expect("Unable to write to stdout");
        let mut username = String::new();
        io::stdin().read_line(&mut username).expect("Unable to read user input");
        username = username.trim().to_owned();

        print!("Password: ");
        io::stdout().flush().expect("Unable to write to stdout");
        let mut password = String::new();
        io::stdin().read_line(&mut password).expect("Unable to read user input");
        password = password.trim().to_owned();

        match client
            .matrix_auth()
            .login_username(&username, &password)
            .initial_device_display_name(INITIAL_DEVICE_DISPLAY_NAME)
            .await
        {
            Ok(_) => {
                println!("Logged in as {username}");
                break;
            }
            Err(error) => {
                println!("Error logging in: {error}");
                println!("Please try again\n");
            }
        }
    }

    Ok(())
}

/// Login with SSO.
async fn login_with_sso(client: &Client, idp: Option<&IdentityProvider>) -> anyhow::Result<()> {
    println!("Logging in with SSO…");

    let mut login_builder = client.matrix_auth().login_sso(|url| async move {
        // Usually we would want to use a library to open the URL in the browser, but
        // let's keep it simple.
        println!("\nOpen this URL in your browser: {url}\n");
        println!("Waiting for login token…");
        Ok(())
    });

    if let Some(idp) = idp {
        login_builder = login_builder.identity_provider_id(&idp.id);
    }

    login_builder.await?;

    println!("Logged in as {}", client.user_id().unwrap());

    Ok(())
}

/// Handle room messages by logging them.
async fn on_room_message(event: OriginalSyncRoomMessageEvent, room: Room) {
    // We only want to listen to joined rooms.
    if room.state() != RoomState::Joined {
        return;
    }

    // We only want to log text messages.
    let MessageType::Text(msgtype) = &event.content.msgtype else {
        return;
    };

    let member = room
        .get_member(&event.sender)
        .await
        .expect("Couldn't get the room member")
        .expect("The room member doesn't exist");
    let name = member.name();

    println!("{name}: {}", msgtype.body);
}
