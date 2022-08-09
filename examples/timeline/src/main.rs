use std::{env, process::exit, sync::Mutex, time::Duration};

use futures::StreamExt;
use futures_signals::signal_vec::SignalVecExt;
use matrix_sdk::{
    self,
    config::SyncSettings,
    room::Room,
    ruma::{
        api::client::filter::{FilterDefinition, LazyLoadOptions},
        events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent},
        uint,
    },
    Client, LoopCtrl,
};
use tokio::sync::oneshot;
use url::Url;

async fn login(homeserver_url: String, username: &str, password: &str) -> Client {
    let homeserver_url = Url::parse(&homeserver_url).expect("Couldn't parse the homeserver URL");
    let client = Client::builder()
        .homeserver_url(homeserver_url)
        .sled_store("./", Some("some password"))
        .await
        .unwrap()
        .build()
        .await
        .unwrap();

    client
        .login_username(username, password)
        .initial_device_display_name("rust-sdk")
        .send()
        .await
        .unwrap();
    client
}

fn _event_content(event: AnySyncTimelineEvent) -> Option<String> {
    if let AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
        SyncMessageLikeEvent::Original(event),
    )) = event
    {
        Some(event.content.msgtype.body().to_owned())
    } else {
        None
    }
}

async fn print_timeline(room: Room) {
    let timeline = room.timeline();
    let mut timeline_stream = timeline.signal().to_stream();
    tokio::spawn(async move {
        while let Some(_diff) = timeline_stream.next().await {
            // Is a straight-forward CLI example of dynamic timeline items
            // possible?? let event = event.unwrap();
            //if let Some(content) =
            // event_content(event.event.deserialize().unwrap()) {
            //    println!("{content}");
            //}
        }
    });

    loop {
        match timeline.paginate_backwards(uint!(10)).await {
            Ok(outcome) if !outcome.more_messages => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("error paginating: {e}");
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let (homeserver_url, username, password, room_id) =
        match (env::args().nth(1), env::args().nth(2), env::args().nth(3), env::args().nth(4)) {
            (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
            _ => {
                eprintln!(
                    "Usage: {} <homeserver_url> <username> <password> <room_id>",
                    env::args().next().unwrap()
                );
                exit(1)
            }
        };

    let client = login(homeserver_url, &username, &password).await;

    let mut filter = FilterDefinition::default();
    filter.room.include_leave = true;
    filter.room.state.lazy_load_options =
        LazyLoadOptions::Enabled { include_redundant_members: false };

    let sync_settings = SyncSettings::new().timeout(Duration::from_secs(30)).filter(filter.into());
    let (sender, receiver) = oneshot::channel::<()>();
    let sender = Mutex::new(Some(sender));
    let client_clone = client.clone();
    tokio::spawn(async move {
        client_clone
            .sync_with_callback(sync_settings, |_| async {
                if let Some(sender) = sender.lock().unwrap().take() {
                    sender.send(()).unwrap();
                }
                LoopCtrl::Continue
            })
            .await
            .unwrap();
    });

    // Wait for the first sync response
    println!("Wait for the first sync");
    receiver.await.unwrap();

    let room = client.get_room(room_id.as_str().try_into().unwrap()).unwrap();

    print_timeline(room).await;

    Ok(())
}
