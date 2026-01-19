#![recursion_limit = "256"]

use std::env;

use anyhow::{Context, anyhow};
use futures_util::StreamExt;
use matrix_sdk::{
    Client,
    RoomState,
    config::SyncSettings,
    ruma::{OwnedServerName, RoomOrAliasId, ServerName},
};
use matrix_sdk_rtc::{LiveKitConnection, LiveKitResult, livekit_service_url};
use matrix_sdk_rtc_livekit::{
    LiveKitRoomOptionsProvider, LiveKitSdkConnector, LiveKitTokenProvider, RoomOptions,
};
use tracing::info;

struct EnvLiveKitTokenProvider {
    token: String,
}

struct DefaultRoomOptionsProvider;

#[async_trait::async_trait]
impl LiveKitTokenProvider for EnvLiveKitTokenProvider {
    async fn token(&self, _room: &matrix_sdk::Room) -> matrix_sdk_rtc::LiveKitResult<String> {
        Ok(self.token.clone())
    }
}

impl LiveKitRoomOptionsProvider for DefaultRoomOptionsProvider {
    fn room_options(&self, _room: &matrix_sdk::Room) -> RoomOptions {
        RoomOptions::default()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let homeserver_url = required_env("HOMESERVER_URL")?;
    let username = required_env("MATRIX_USERNAME")?;
    let password = required_env("MATRIX_PASSWORD")?;
    let room_id_or_alias = required_env("ROOM_ID")?;
    let livekit_token = required_env("LIVEKIT_TOKEN")?;
    let livekit_service_url_override = optional_env("LIVEKIT_SERVICE_URL");

    let client = Client::builder()
        .homeserver_url(homeserver_url)
        .build()
        .await
        .context("build Matrix client")?;

    client
        .matrix_auth()
        .login_username(&username, &password)
        .await
        .context("login Matrix user")?;

    let room_id_or_alias = RoomOrAliasId::parse(room_id_or_alias).context("parse ROOM_ID")?;
    let via_servers = via_servers_from_env().context("parse VIA_SERVERS")?;
    let room = match room_id_or_alias.clone().try_into() {
        Ok(room_id) => match client.get_room(&room_id) {
            Some(room) if room.state() == RoomState::Joined => room,
            _ => client
                .join_room_by_id(&room_id)
                .await
                .context("join room")?,
        },
        Err(_) => client
            .join_room_by_id_or_alias(&room_id_or_alias, &via_servers)
            .await
            .context("join room")?,
    };

    let sync_client = client.clone();
    let sync_handle = tokio::spawn(async move {
        sync_client.sync(SyncSettings::new()).await
    });

    // NOTE: Joining a call requires publishing MatrixRTC memberships (m.call.member) for
    // this device. This example does not implement that step; you can integrate your own
    // membership publisher (or Element Call) before starting the driver so that the room
    // contains active call memberships.

    let token_provider = EnvLiveKitTokenProvider { token: livekit_token };
    let connector = LiveKitSdkConnector::new(token_provider, DefaultRoomOptionsProvider);

    let service_url = match livekit_service_url_override {
        Some(url) => url,
        None => livekit_service_url(&client)
            .await
            .context("fetch LiveKit service url")?,
    };
    run_livekit_driver(room, connector, service_url)
        .await
        .context("run LiveKit room driver")?;

    sync_handle.abort();

    Ok(())
}

fn required_env(name: &str) -> anyhow::Result<String> {
    env::var(name).with_context(|| anyhow!("missing required env var: {name}"))
}

fn optional_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn via_servers_from_env() -> anyhow::Result<Vec<OwnedServerName>> {
    let value = match env::var("VIA_SERVERS") {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };

    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| ServerName::parse(entry).context("parse server name"))
        .collect()
}

async fn run_livekit_driver<C>(
    room: matrix_sdk::Room,
    connector: C,
    service_url: String,
) -> LiveKitResult<()>
where
    C: matrix_sdk_rtc::LiveKitConnector,
{
    let mut connection = None;
    let mut info_stream = room.subscribe_info();

    update_connection(&room, &connector, &service_url, &room.clone_info(), &mut connection).await?;

    while let Some(room_info) = info_stream.next().await {
        update_connection(&room, &connector, &service_url, &room_info, &mut connection).await?;
    }

    if let Some(connection) = connection.take() {
        connection.disconnect().await?;
    }

    Ok(())
}

async fn update_connection<C>(
    room: &matrix_sdk::Room,
    connector: &C,
    service_url: &str,
    room_info: &matrix_sdk::RoomInfo,
    connection: &mut Option<C::Connection>,
) -> LiveKitResult<()>
where
    C: matrix_sdk_rtc::LiveKitConnector,
{
    let has_memberships = room_info.has_active_room_call();

    if has_memberships {
        if connection.is_none() {
            info!(room_id = ?room.room_id(), "joining LiveKit room for active call");
            let new_connection = connector.connect(service_url, room).await?;
            *connection = Some(new_connection);
        }
    } else if let Some(existing) = connection.take() {
        info!(room_id = ?room.room_id(), "leaving LiveKit room because the call ended");
        existing.disconnect().await?;
    }

    Ok(())
}
