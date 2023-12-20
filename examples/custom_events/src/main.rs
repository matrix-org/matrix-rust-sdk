///
///  This is an example showcasing how to build a very simple bot with custom
/// events  using the matrix-sdk. To try it, you need a rust build setup, then
/// you can run: `cargo run -p example-custom-events -- <homeserver_url> <user>
/// <password>`
///
/// Use a second client to open a DM to your bot or invite them into some room.
/// You should see it automatically join. Then post `!ping`  and observe the log
/// of the bot. You will see that it sends the `Ping` event and upon receiving
/// it responds with the `Ack` event send to the room. You won't see that in
/// most regular clients, unless you activate showing of unknown events.
use std::{env, process::exit};

use matrix_sdk::{
    config::SyncSettings,
    ruma::{
        events::{
            macros::EventContent,
            room::{
                member::StrippedRoomMemberEvent,
                message::{MessageType, OriginalSyncRoomMessageEvent},
            },
        },
        OwnedEventId,
    },
    Client, Room, RoomState,
};
use serde::{Deserialize, Serialize};
use tokio::time::{sleep, Duration};

// We use ruma to define our custom events. Just declare the events content
// by deriving from `EventContent` and define `ruma_events` for the metadata

#[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
#[ruma_event(type = "rs.matrix-sdk.example.ping", kind = MessageLike)]
pub struct PingEventContent {}

#[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
#[ruma_event(type = "rs.matrix-sdk.example.ack", kind = MessageLike)]
pub struct AckEventContent {
    // the event ID of the ping.
    ping_id: OwnedEventId,
}

// Deriving `EventContent` generates a few types and aliases,
// like wrapping the content into full-blown events: for `PingEventContent` this
// generates us `PingEvent` and `SyncPingEvent`, which have redaction support
// and contain all the other event-metadata like event_id and room_id. We will
// use that for `on_ping_event`.

// we want to start the ping-ack-flow on "!ping" messages.
async fn on_regular_room_message(event: OriginalSyncRoomMessageEvent, room: Room) {
    if room.state() != RoomState::Joined {
        return;
    }
    let MessageType::Text(text_content) = event.content.msgtype else { return };

    if text_content.body.contains("!ping") {
        let content = PingEventContent {};

        println!("sending ping");
        room.send(content).await.unwrap();
        println!("ping sent");
    }
}

// call this on any PingEvent we receive
async fn on_ping_event(event: SyncPingEvent, room: Room) {
    if room.state() != RoomState::Joined {
        return;
    }

    let event_id = event.event_id().to_owned();

    // Send an ack with the event_id of the ping, as our 'protocol' demands
    let content = AckEventContent { ping_id: event_id };
    println!("sending ack");
    room.send(content).await.unwrap();

    println!("ack sent");
}

// once logged in, this is called where we configure the handlers
// and run the client
async fn sync_loop(client: Client) -> anyhow::Result<()> {
    // invite acceptance as in the getting-started-client
    client.add_event_handler(on_stripped_state_member);
    let response = client.sync_once(SyncSettings::default()).await.unwrap();

    // our customisation:
    //  - send `PingEvent` on `!ping` in any room
    client.add_event_handler(on_regular_room_message);
    //  - send `AckEvent` on `PingEvent` in any room
    client.add_event_handler(on_ping_event);

    let settings = SyncSettings::default().token(response.next_batch);
    client.sync(settings).await?; // this essentially loops until we kill the bot

    Ok(())
}

// ------ below is mainly like the getting-started example, see that for docs.

async fn login_and_sync(
    homeserver_url: String,
    username: &str,
    password: &str,
) -> anyhow::Result<()> {
    let client = Client::builder().homeserver_url(homeserver_url).build().await?;
    client
        .matrix_auth()
        .login_username(username, password)
        .initial_device_display_name("getting started bot")
        .await?;

    // it worked!
    println!("logged in as {username}");
    sync_loop(client).await
}

// Whenever we see a new stripped room member event, we've asked our client to
// call this function. So what exactly are we doing then?
async fn on_stripped_state_member(
    room_member: StrippedRoomMemberEvent,
    client: Client,
    room: Room,
) {
    if room_member.state_key != client.user_id().unwrap() {
        // the invite we've seen isn't for us, but for someone else. ignore
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // set up some simple stderr logging. You can configure it by changing the env
    // var `RUST_LOG`
    tracing_subscriber::fmt::init();

    // parse the command line for homeserver, username and password
    let (homeserver_url, username, password) =
        match (env::args().nth(1), env::args().nth(2), env::args().nth(3)) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => {
                eprintln!(
                    "Usage: {} <homeserver_url> <username> <password>",
                    env::args().next().unwrap()
                );
                // exist if missing
                exit(1)
            }
        };

    // our actual runner
    login_and_sync(homeserver_url, &username, &password).await?;
    Ok(())
}
