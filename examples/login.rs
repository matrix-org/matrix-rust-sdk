#![feature(async_closure)]

use std::{env, process::exit};
use std::pin::Pin;
use std::future::Future;
use std::rc::Rc;
use std::cell::RefCell;

use matrix_nio::{
    self,
    events::{
        collections::all::RoomEvent,
        room::message::{MessageEvent, MessageEventContent, TextMessageEventContent},
        EventType,
    },
    AsyncClient, AsyncClientConfig, SyncSettings, Room
};

async fn async_helper(room: Rc<RefCell<Room>>, event: Rc<RoomEvent>) {
    let room = room.borrow();
    if let RoomEvent::RoomMessage(MessageEvent {
        content: MessageEventContent::Text(TextMessageEventContent { body: msg_body, .. }),
        sender,
        ..
    }) = &*event
    {
        let user = room.members.get(&sender.to_string()).unwrap();
        println!("{}: {}", user.display_name.as_ref().unwrap_or(&sender.to_string()), msg_body);
    }

}

fn async_callback(room: Rc<RefCell<Room>>, event: Rc<RoomEvent>) -> Pin<Box<dyn Future<Output = ()>>> {
    Box::pin(async_helper(room, event))
}

async fn login(
    homeserver_url: String,
    username: String,
    password: String,
) -> Result<(), matrix_nio::Error> {
    let client_config = AsyncClientConfig::new()
        .proxy("http://localhost:8080")?
        .disable_ssl_verification();
    let mut client = AsyncClient::new_with_config(&homeserver_url, None, client_config).unwrap();

    client.add_event_future(EventType::RoomMessage, Box::new(async_callback));

    client.login(username, password, None).await?;
    let response = client.sync(SyncSettings::new()).await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), matrix_nio::Error> {
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

    login(homeserver_url, username, password).await
}
