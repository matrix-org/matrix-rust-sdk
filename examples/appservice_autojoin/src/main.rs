use std::env;

use matrix_sdk_appservice::{
    matrix_sdk::{
        event_handler::Ctx,
        room,
        ruma::{
            events::room::member::{MembershipState, OriginalSyncRoomMemberEvent},
            UserId,
        },
    },
    ruma::api::client::error::ErrorKind,
    AppService, AppServiceBuilder, AppServiceRegistration, Result,
};
use tracing::trace;

pub async fn handle_room_member(
    appservice: AppService,
    room: room::Common,
    event: OriginalSyncRoomMemberEvent,
) -> Result<()> {
    if !appservice.user_id_is_in_namespace(&event.state_key) {
        trace!("not an appservice user: {}", event.state_key);
    } else if let MembershipState::Invite = event.content.membership {
        let user_id = UserId::parse(event.state_key.as_str())?;
        if let Err(error) = appservice.register_user(user_id.localpart(), None).await {
            error_if_user_not_in_use(error)?;
        }

        let client = appservice.user(Some(user_id.localpart())).await?;
        client.join_room_by_id(room.room_id()).await?;
    }

    Ok(())
}

pub fn error_if_user_not_in_use(error: matrix_sdk_appservice::Error) -> Result<()> {
    // FIXME: Use if-let chain once available
    match &error {
        // If user is already in use that's OK.
        matrix_sdk_appservice::Error::Matrix(err)
            if err.client_api_error_kind() == Some(&ErrorKind::UserInUse) =>
        {
            Ok(())
        }
        // In all other cases return with an error.
        _ => Err(error),
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

    let user = appservice.user(None).await?;

    user.add_event_handler_context(appservice.clone());
    user.add_event_handler(
        move |event: OriginalSyncRoomMemberEvent,
              room: room::Common,
              Ctx(appservice): Ctx<AppService>| {
            handle_room_member(appservice, room, event)
        },
    );

    let (host, port) = appservice.registration().get_host_and_port()?;
    appservice.run(host, port).await?;

    Ok(())
}
