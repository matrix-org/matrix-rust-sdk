use std::{env, process::exit, sync::Mutex, time::Duration};

use futures::{pin_mut, StreamExt};
use matrix_sdk::{
    self,
    config::SyncSettings,
    room::Room,
    ruma::{
        api::client::filter::{FilterDefinition, LazyLoadOptions, RoomEventFilter, RoomFilter},
        assign,
        events::{AnyMessageLikeEventContent, AnySyncRoomEvent},
    },
    store::make_store_config,
    Client, LoopCtrl,
};
use tokio::sync::oneshot;
use url::Url;

async fn login(homeserver_url: String, username: &str, password: &str) -> Client {
    let homeserver_url = Url::parse(&homeserver_url).expect("Couldn't parse the homeserver URL");
    let path = "./";
    let store_config = make_store_config(path, Some("some password")).unwrap();
    let client = Client::builder()
        .homeserver_url(homeserver_url)
        .store_config(store_config)
        .build()
        .await
        .unwrap();

    client.login(username, password, None, Some("rust-sdk")).await.unwrap();
    client
}

fn event_content(event: AnySyncRoomEvent) -> Option<String> {
    if let AnySyncRoomEvent::MessageLike(event) = event {
        if let AnyMessageLikeEventContent::RoomMessage(content) = event.content() {
            return Some(content.msgtype.body().to_owned());
        }
    }
    None
}
async fn print_timeline(room: Room) {
    let backward_stream = room.timeline_backward().await.unwrap();

    pin_mut!(backward_stream);

    while let Some(event) = backward_stream.next().await {
        let event = event.unwrap();
        if let Some(content) = event_content(event.event.deserialize().unwrap()) {
            println!("{}", content);
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), matrix_sdk::Error> {
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

    let room_event_filter = assign!(RoomEventFilter::default(), {
        lazy_load_options: LazyLoadOptions::Enabled {include_redundant_members: false},
    });
    let filter = assign!(FilterDefinition::default(), {
        room: assign!(RoomFilter::empty(), {
            include_leave: true,
            state: room_event_filter,
        }),
    });

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
            .await;
    });

    // Wait for the first sync response
    println!("Wait for the first sync");
    receiver.await.unwrap();

    let room = client.get_room(room_id.as_str().try_into().unwrap()).unwrap();

    print_timeline(room).await;

    Ok(())
}
