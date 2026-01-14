use std::env;

use anyhow::{Context, anyhow};
use matrix_sdk::{Client, config::SyncSettings, ruma::RoomId};
use matrix_sdk_rtc::LiveKitRoomDriver;
use matrix_sdk_rtc_livekit::{LiveKitSdkConnector, LiveKitTokenProvider, RoomOptions};

struct EnvLiveKitTokenProvider {
    token: String,
}

#[async_trait::async_trait]
impl LiveKitTokenProvider for EnvLiveKitTokenProvider {
    async fn token(&self, _room: &matrix_sdk::Room) -> matrix_sdk_rtc::LiveKitResult<String> {
        Ok(self.token.clone())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let homeserver_url = required_env("HOMESERVER_URL")?;
    let username = required_env("MATRIX_USERNAME")?;
    let password = required_env("MATRIX_PASSWORD")?;
    let room_id = required_env("ROOM_ID")?;
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

    let room_id = RoomId::parse(room_id).context("parse ROOM_ID")?;
    let room = match client.get_joined_room(&room_id) {
        Some(room) => room,
        None => client
            .join_room_by_id(&room_id)
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
    let room_options = |_| RoomOptions::default();
    let connector = LiveKitSdkConnector::new(token_provider, room_options);

    let driver = LiveKitRoomDriver::new(room, connector);
    driver.run().await.context("run LiveKit room driver")?;

    sync_handle.abort();

    Ok(())
}

fn required_env(name: &str) -> anyhow::Result<String> {
    env::var(name).with_context(|| anyhow!("missing required env var: {name}"))
}
