use std::env;

use matrix_sdk_appservice::{
    matrix_sdk::{
        event_handler::Ctx,
        room::Room,
        ruma::{
            events::room::member::{MembershipState, OriginalSyncRoomMemberEvent},
            UserId,
        },
        HttpError,
    },
    AppService, AppServiceRegistration, Result,
};
use ruma::api::{
    client::{error::ErrorKind, uiaa::UiaaResponse},
    error::{FromHttpResponseError, ServerError},
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
    match error {
        // If user is already in use that's OK.
        matrix_sdk_appservice::Error::Matrix(matrix_sdk::Error::Http(HttpError::UiaaError(
            FromHttpResponseError::Server(ServerError::Known(UiaaResponse::MatrixError(error))),
        ))) if matches!(error.kind, ErrorKind::UserInUse) => Ok(()),
        // In all other cases return with an error.
        error => Err(error),
    }
}

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env::set_var("RUST_LOG", "matrix_sdk=debug,matrix_sdk_appservice=debug");
    tracing_subscriber::fmt::init();

    let homeserver_url = "http://localhost:8008";
    let server_name = "localhost";
    let registration = AppServiceRegistration::try_from_yaml_file("./tests/registration.yaml")?;

    let appservice = AppService::new(homeserver_url, server_name, registration).await?;
    appservice.register_user_query(Box::new(|_, _| Box::pin(async { true }))).await;
    appservice
        .virtual_user(None)
        .await?
        .register_event_handler_context(appservice.clone())
        .register_event_handler(
            move |event: OriginalSyncRoomMemberEvent,
                  room: Room,
                  Ctx(appservice): Ctx<AppService>| {
                handle_room_member(appservice, room, event)
            },
        )
        .await;

    let (host, port) = appservice.registration().get_host_and_port()?;
    appservice.run(host, port).await?;

    Ok(())
}
