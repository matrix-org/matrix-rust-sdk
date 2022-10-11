use std::env;

use matrix_sdk_appservice::{
    matrix_sdk::{
        event_handler::Ctx,
        room::Room,
        ruma::{
            events::room::member::{MembershipState, OriginalSyncRoomMemberEvent},
            UserId,
        },
        IsError,
    },
    ruma::api::client::{error::ErrorKind, Error as ClientError},
    AppService, AppServiceBuilder, AppServiceRegistration, Result,
};
use tracing::trace;

pub async fn handle_room_member(
    appservice: AppService,
    room: Room,
    event: OriginalSyncRoomMemberEvent,
) -> Result<()> {
    if !appservice.user_id_is_in_namespace(&event.state_key) {
        trace!("not an appservice user: {}", event.state_key);
    } else if let MembershipState::Invite = event.content.membership {
        let user_id = UserId::parse(event.state_key.as_str())?;
        if let Err(error) = appservice.register_virtual_user(user_id.localpart(), None).await {
            error_if_user_not_in_use(error)?;
        }

        let client = appservice.virtual_user(Some(user_id.localpart())).await?;
        client.join_room_by_id(room.room_id()).await?;
    }

    Ok(())
}

pub fn error_if_user_not_in_use(error: matrix_sdk_appservice::Error) -> Result<()> {
    if let Some(&ClientError { kind: ErrorKind::UserInUse, .. }) = error.as_error() {
        // If user is already in use that's OK.
        Ok(())
    } else {
        // In all other cases return with an error.
        Err(error)
    }
}

#[tokio::main]
pub async fn main() -> anyhow::Result<()> {
    env::set_var("RUST_LOG", "matrix_sdk=debug,matrix_sdk_appservice=debug");
    tracing_subscriber::fmt::init();

    let homeserver_url = "http://localhost:8008";
    let server_name = "localhost";
    let registration =
        AppServiceRegistration::try_from_yaml_file("./appservice-registration.yaml")?;
    let appservice =
        AppServiceBuilder::new(homeserver_url.parse()?, server_name.parse()?, registration)
            .build()
            .await?;

    appservice.register_user_query(Box::new(|_, _| Box::pin(async { true }))).await;

    let virtual_user = appservice.virtual_user(None).await?;

    virtual_user.add_event_handler_context(appservice.clone());
    virtual_user.add_event_handler(
        move |event: OriginalSyncRoomMemberEvent, room: Room, Ctx(appservice): Ctx<AppService>| {
            handle_room_member(appservice, room, event)
        },
    );

    let (host, port) = appservice.registration().get_host_and_port()?;
    appservice.run(host, port).await?;

    Ok(())
}
