use std::{convert::TryFrom, env};

use matrix_sdk_appservice::{
    sdk::{
        async_trait,
        events::{
            room::member::{MemberEventContent, MembershipState},
            SyncStateEvent,
        },
        identifiers::UserId,
        room::Room,
        EventHandler,
    },
    Appservice, AppserviceRegistration,
};
use tracing::{error, trace};

struct AppserviceEventHandler {
    appservice: Appservice,
}

impl AppserviceEventHandler {
    pub fn new(appservice: Appservice) -> Self {
        Self { appservice }
    }

    pub async fn handle_room_member(
        &self,
        room: Room,
        event: &SyncStateEvent<MemberEventContent>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !self.appservice.user_id_is_in_namespace(&event.state_key)? {
            trace!("not an appservice user: {}", event.state_key);
        } else if let MembershipState::Invite = event.content.membership {
            let user_id = UserId::try_from(event.state_key.clone())?;

            let appservice = self.appservice.clone();
            appservice.register_virtual_user(user_id.localpart()).await?;

            let client = appservice.virtual_user_client(user_id.localpart()).await?;

            client.join_room_by_id(room.room_id()).await?;
        }

        Ok(())
    }
}

#[async_trait]
impl EventHandler for AppserviceEventHandler {
    async fn on_room_member(&self, room: Room, event: &SyncStateEvent<MemberEventContent>) {
        match self.handle_room_member(room, event).await {
            Ok(_) => (),
            Err(error) => error!("{:?}", error),
        }
    }
}

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env::set_var("RUST_LOG", "matrix_sdk=debug,matrix_sdk_appservice=debug");
    tracing_subscriber::fmt::init();

    let homeserver_url = "http://localhost:8008";
    let server_name = "localhost";
    let registration = AppserviceRegistration::try_from_yaml_file("./tests/registration.yaml")?;

    let mut appservice = Appservice::new(homeserver_url, server_name, registration).await?;
    appservice.set_event_handler(Box::new(AppserviceEventHandler::new(appservice.clone()))).await?;

    let (host, port) = appservice.registration().get_host_and_port()?;
    appservice.run(host, port).await?;

    Ok(())
}
