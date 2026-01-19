#![recursion_limit = "256"]

use std::env;

use anyhow::{Context, anyhow};
use matrix_sdk::{
    Client,
    RoomState,
    config::SyncSettings,
    ruma::{OwnedServerName, RoomId, RoomOrAliasId, ServerName},
};
use matrix_sdk_rtc::{LiveKitConnection, LiveKitResult, livekit_service_url};
use matrix_sdk_rtc_livekit::{
    LiveKitRoomOptionsProvider, LiveKitSdkConnector, LiveKitTokenProvider, RoomOptions,
};
use ruma::api::client::account::request_openid_token;
use serde_json::Value as JsonValue;
use tracing::info;

struct EnvLiveKitTokenProvider {
    token: String,
}

struct DefaultRoomOptionsProvider;

#[async_trait::async_trait]
impl LiveKitTokenProvider for EnvLiveKitTokenProvider {
    async fn token(&self, _room: &matrix_sdk::Room) -> LiveKitResult<String> {
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
    let livekit_service_url_override = optional_env("LIVEKIT_SERVICE_URL");
    let livekit_sfu_get_url = optional_env("LIVEKIT_SFU_GET_URL");

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
    let room = match RoomId::parse(room_id_or_alias.as_str()) {
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

    let (service_url, livekit_token) = if let Some(sfu_url) = livekit_sfu_get_url {
        let openid_token = request_openid_token(&client)
            .await
            .context("request OpenID token")?;
        let device_id = client
            .device_id()
            .context("missing device id for /sfu/get request")?
            .to_string();
        fetch_sfu_token(&sfu_url, room.room_id().to_owned(), device_id, &openid_token)
            .await
            .context("fetch LiveKit token from /sfu/get")?
    } else {
        let token = required_env("LIVEKIT_TOKEN")?;
        let service_url = match livekit_service_url_override {
            Some(url) => url,
            None => livekit_service_url(&client)
                .await
                .context("fetch LiveKit service url")?,
        };
        (service_url, token)
    };

    let token_provider = EnvLiveKitTokenProvider { token: livekit_token };
    let connector = LiveKitSdkConnector::new(token_provider, DefaultRoomOptionsProvider);

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

async fn request_openid_token(
    client: &Client,
) -> anyhow::Result<request_openid_token::v3::Response> {
    let user_id = client
        .user_id()
        .context("missing user id for OpenID token request")?;
    let request = request_openid_token::v3::Request::new(user_id.to_owned());
    let response = client.send(request).await?;
    Ok(response)
}

#[derive(serde::Serialize)]
struct SfuGetRequest {
    room: String,
    openid_token: OpenIdToken,
    device_id: String,
}

#[derive(serde::Serialize)]
struct OpenIdToken {
    access_token: String,
    expires_in: u64,
    matrix_server_name: String,
    token_type: String,
}

async fn fetch_sfu_token(
    url: &str,
    room_id: matrix_sdk::ruma::OwnedRoomId,
    device_id: String,
    openid_token: &request_openid_token::v3::Response,
) -> anyhow::Result<(String, String)> {
    let request_body = SfuGetRequest {
        room: room_id.to_string(),
        openid_token: OpenIdToken {
            access_token: openid_token.access_token.clone(),
            expires_in: openid_token.expires_in.as_secs(),
            matrix_server_name: openid_token.matrix_server_name.to_string(),
            token_type: openid_token.token_type.to_string(),
        },
        device_id,
    };
    let client = reqwest::Client::new();
    let request = client.post(url).json(&request_body);

    let response = request.send().await?.error_for_status()?;
    let payload: JsonValue = response.json().await?;

    let service_url = extract_string(
        &payload,
        &[
            "service_url",
            "livekit_service_url",
            "livekit_url",
            "sfu_base_url",
            "sfu_url",
            "url",
        ],
    )
    .context("missing LiveKit service url in /sfu/get response")?;
    let token = extract_string(&payload, &["token", "jwt", "access_token"])
        .context("missing LiveKit token in /sfu/get response")?;

    Ok((service_url, token))
}

fn extract_string(payload: &JsonValue, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        payload
            .get(*key)
            .and_then(|value| value.as_str())
            .map(|value| value.to_owned())
    })
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
