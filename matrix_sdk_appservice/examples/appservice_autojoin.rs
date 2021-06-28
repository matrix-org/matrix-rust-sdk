use std::{convert::TryFrom, env};

use matrix_sdk_appservice::{
    matrix_sdk::{
        async_trait,
        room::Room,
        ruma::{
            events::{
                room::member::{MemberEventContent, MembershipState},
                SyncStateEvent,
            },
            UserId,
        },
        EventHandler,
    },
    AppService, AppServiceRegistration,
};
use tracing::{error, trace};

struct AppServiceEventHandler {
    appservice: AppService,
}

impl AppServiceEventHandler {
    pub fn new(appservice: AppService) -> Self {
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
            let localpart = user_id.localpart();
            self.appservice.virtual_user_client(localpart).await?;
            self.appservice.join_room_by_id(Some(localpart), room.room_id()).await?;
        }

        Ok(())
    }
}

#[async_trait]
impl EventHandler for AppServiceEventHandler {
    async fn on_room_member(&self, room: Room, event: &SyncStateEvent<MemberEventContent>) {
        match self.handle_room_member(room, event).await {
            Ok(_) => (),
            Err(error) => error!("{:?}", error),
        }
    }
}

#[cfg_attr(feature = "warp", tokio::main)]
#[cfg_attr(feature = "actix", actix_rt::main)]
pub async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env::set_var("RUST_LOG", "matrix_sdk=trace,matrix_sdk_appservice=trace");
    tracing_subscriber::fmt::init();

    let homeserver_url = "http://localhost:8008";
    let server_name = "localhost";
    let registration = AppServiceRegistration::try_from_yaml_file("./tests/registration.yaml")?;

    let mut appservice = AppService::new(homeserver_url, server_name, registration).await?;
    appservice.set_event_handler(Box::new(AppServiceEventHandler::new(appservice.clone()))).await?;

    let (host, port) = appservice.registration().get_host_and_port()?;
    appservice.run(host, port).await?;

    Ok(())
}
