#![recursion_limit = "256"]

use std::env;

use anyhow::{Context, anyhow};
use matrix_sdk::{
    Client,
    RoomState,
    config::SyncSettings,
    ruma::{OwnedServerName, RoomOrAliasId, ServerName},
};
use matrix_sdk_rtc::LiveKitRoomDriver;
use matrix_sdk_rtc_livekit::{
    LiveKitRoomOptionsProvider, LiveKitSdkConnector, LiveKitTokenProvider, RoomOptions,
};

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
    let room = match room_id_or_alias.as_room_id() {
        Some(room_id) => match client.get_room(room_id) {
            Some(room) if room.state() == RoomState::Joined => room,
            _ => client
                .join_room_by_id(room_id)
                .await
                .context("join room")?,
        },
        None => client
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

    let driver = LiveKitRoomDriver::new(room, connector);
    driver.run().await.context("run LiveKit room driver")?;

    sync_handle.abort();

    Ok(())
}

fn required_env(name: &str) -> anyhow::Result<String> {
    env::var(name).with_context(|| anyhow!("missing required env var: {name}"))
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
