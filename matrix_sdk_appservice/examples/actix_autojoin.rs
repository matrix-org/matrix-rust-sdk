use std::{convert::TryFrom, env};

use actix_web::{App, HttpServer};
use matrix_sdk::{
    async_trait,
    events::{
        room::member::{MemberEventContent, MembershipState},
        SyncStateEvent,
    },
    identifiers::UserId,
    room::Room,
    EventHandler,
};
use matrix_sdk_appservice::{Appservice, AppserviceRegistration};

struct AppserviceEventHandler {
    appservice: Appservice,
}

impl AppserviceEventHandler {
    pub fn new(appservice: Appservice) -> Self {
        Self { appservice }
    }
}

#[async_trait]
impl EventHandler for AppserviceEventHandler {
    async fn on_room_member(&self, room: Room, event: &SyncStateEvent<MemberEventContent>) {
        if !self.appservice.user_id_is_in_namespace(&event.state_key).unwrap() {
            dbg!("not an appservice user");
            return;
        }

        if let MembershipState::Invite = event.content.membership {
            let user_id = UserId::try_from(event.state_key.clone()).unwrap();

            let appservice = self.appservice.clone();
            appservice.register_virtual_user(user_id.localpart()).await.unwrap();

            let client = appservice.virtual_user(user_id.localpart()).await.unwrap();

            client.join_room_by_id(room.room_id()).await.unwrap();
        }
    }
}

#[actix_web::main]
pub async fn main() -> std::io::Result<()> {
    env::set_var("RUST_LOG", "actix_web=debug,actix_server=info,matrix_sdk=debug");
    tracing_subscriber::fmt::init();

    let homeserver_url = "http://localhost:8008";
    let server_name = "localhost";
    let registration =
        AppserviceRegistration::try_from_yaml_file("./tests/registration.yaml").unwrap();

    let mut appservice = Appservice::new(homeserver_url, server_name, registration).await.unwrap();

    let event_handler = AppserviceEventHandler::new(appservice.clone());

    appservice.set_event_handler(Box::new(event_handler)).await.unwrap();

    HttpServer::new(move || App::new().service(appservice.actix_service()))
        .bind(("0.0.0.0", 8090))?
        .run()
        .await
}
