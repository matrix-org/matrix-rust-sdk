#![recursion_limit = "256"]

use std::env;

use anyhow::{Context, anyhow};
use matrix_sdk::{
    Client,
    RoomState,
    RoomMemberships,
    config::SyncSettings,
    ruma::{OwnedServerName, RoomId, RoomOrAliasId, ServerName},
};
use matrix_sdk_rtc::{LiveKitConnector, LiveKitResult, livekit_service_url};
#[cfg(all(feature = "v4l2", target_os = "linux"))]
use matrix_sdk_rtc::LiveKitError;
use matrix_sdk_rtc_livekit::{
    LiveKitRoomOptionsProvider, LiveKitSdkConnector, LiveKitTokenProvider, RoomOptions, Room,
};
#[cfg(feature = "e2ee-per-participant")]
use matrix_sdk_rtc_livekit::livekit::e2ee::{
    E2eeOptions, EncryptionType,
    key_provider::{KeyProvider, KeyProviderOptions},
};
use ruma::api::client::account::request_openid_token;
#[cfg(feature = "e2ee-per-participant")]
use ruma::serde::Raw;
use serde_json::Value as JsonValue;
#[cfg(feature = "e2ee-per-participant")]
use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD};
#[cfg(feature = "e2ee-per-participant")]
use matrix_sdk_base::crypto::CollectStrategy;
#[cfg(feature = "e2ee-per-participant")]
use sha2::{Digest, Sha256};
use tracing::info;
#[cfg(all(feature = "v4l2", target_os = "linux"))]
use tracing::warn;
use url::Url;

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

#[cfg(feature = "e2ee-per-participant")]
#[derive(Clone)]
struct PerParticipantE2eeContext {
    key_provider: KeyProvider,
    key_index: i32,
    local_key: Vec<u8>,
}

#[cfg(feature = "e2ee-per-participant")]
#[derive(Clone)]
struct E2eeRoomOptionsProvider {
    e2ee: Option<PerParticipantE2eeContext>,
}

#[cfg(feature = "e2ee-per-participant")]
impl LiveKitRoomOptionsProvider for E2eeRoomOptionsProvider {
    fn room_options(&self, _room: &matrix_sdk::Room) -> RoomOptions {
        let mut options = RoomOptions::default();
        if let Some(context) = &self.e2ee {
            options.encryption = Some(E2eeOptions {
                encryption_type: EncryptionType::Gcm,
                key_provider: context.key_provider.clone(),
            });
        }
        options
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
#[derive(Clone, Debug)]
struct V4l2Config {
    device: String,
    width: Option<u32>,
    height: Option<u32>,
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
#[derive(Debug)]
struct V4l2PublishError(anyhow::Error);

#[cfg(all(feature = "v4l2", target_os = "linux"))]
impl std::fmt::Display for V4l2PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
impl std::error::Error for V4l2PublishError {}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
struct V4l2CameraPublisher {
    room: std::sync::Arc<Room>,
    track: matrix_sdk_rtc_livekit::livekit::track::LocalVideoTrack,
    stop_tx: std::sync::mpsc::Sender<()>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
#[derive(Copy, Clone, Debug)]
enum V4l2PixelFormat {
    Nv12,
    Yuyv,
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
impl V4l2CameraPublisher {
    async fn start(room: std::sync::Arc<Room>, config: V4l2Config) -> anyhow::Result<Self> {
        use matrix_sdk_rtc_livekit::livekit::options::{TrackPublishOptions, VideoCodec};
        use matrix_sdk_rtc_livekit::livekit::prelude::*;
        use matrix_sdk_rtc_livekit::livekit::track::{LocalTrack, TrackSource};
        use matrix_sdk_rtc_livekit::livekit::webrtc::prelude::{RtcVideoSource, VideoResolution};
        use matrix_sdk_rtc_livekit::livekit::webrtc::video_source::native::NativeVideoSource;

        let (resolution, pixel_format, rtc_source, mut device) =
            configure_v4l2_device(&config).context("configure V4L2 device")?;

        let track = matrix_sdk_rtc_livekit::livekit::track::LocalVideoTrack::create_video_track(
            "v4l2_camera",
            RtcVideoSource::Native(rtc_source.clone()),
        );

        info!(
            room_name = %room.name(),
            "publishing V4L2 camera track"
        );
        room.local_participant()
            .publish_track(
                LocalTrack::Video(track.clone()),
                TrackPublishOptions {
                    source: TrackSource::Camera,
                    video_codec: VideoCodec::VP8,
                    ..Default::default()
                },
            )
            .await
            .context("publish V4L2 camera track")?;

        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        let task = tokio::task::spawn_blocking(move || {
            run_v4l2_capture_loop(&mut device, resolution, pixel_format, rtc_source, stop_rx)
        });

        Ok(Self { room, track, stop_tx, task })
    }

    async fn stop(self) -> anyhow::Result<()> {
        info!(room_name = %self.room.name(), "stopping V4L2 camera track");
        let _ = self.stop_tx.send(());
        let _ = self.task.await?;
        self.room
            .local_participant()
            .unpublish_track(&self.track.sid())
            .await
            .context("unpublish V4L2 camera track")?;
        Ok(())
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn configure_v4l2_device(
    config: &V4l2Config,
) -> anyhow::Result<(
    matrix_sdk_rtc_livekit::livekit::webrtc::prelude::VideoResolution,
    V4l2PixelFormat,
    matrix_sdk_rtc_livekit::livekit::webrtc::video_source::native::NativeVideoSource,
    v4l::Device,
)> {
    use matrix_sdk_rtc_livekit::livekit::webrtc::prelude::VideoResolution;
    use matrix_sdk_rtc_livekit::livekit::webrtc::video_source::native::NativeVideoSource;
    use v4l::video::Capture;
    use v4l::{Device, FourCC};

    let mut device = Device::with_path(&config.device).context("open V4L2 device")?;
    let mut format = device.format().context("read V4L2 format")?;

    if let Some(width) = config.width {
        format.width = width;
    }
    if let Some(height) = config.height {
        format.height = height;
    }
    let format = set_format_with_fallback(&mut device, format)?;
    let pixel_format = match &format.fourcc.repr {
        b"NV12" => V4l2PixelFormat::Nv12,
        b"YUYV" => V4l2PixelFormat::Yuyv,
        _ => {
            return Err(anyhow!(
                "V4L2 device did not accept NV12 or YUYV; got {:?} instead",
                format.fourcc
            ));
        }
    };

    let resolution = VideoResolution { width: format.width, height: format.height };
    info!(
        device = %config.device,
        width = format.width,
        height = format.height,
        fourcc = ?format.fourcc,
        "configured V4L2 device format"
    );
    let rtc_source = NativeVideoSource::new(resolution.clone());
    Ok((resolution, pixel_format, rtc_source, device))
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn run_v4l2_capture_loop(
    device: &mut v4l::Device,
    resolution: matrix_sdk_rtc_livekit::livekit::webrtc::prelude::VideoResolution,
    pixel_format: V4l2PixelFormat,
    rtc_source: matrix_sdk_rtc_livekit::livekit::webrtc::video_source::native::NativeVideoSource,
    stop_rx: std::sync::mpsc::Receiver<()>,
) -> anyhow::Result<()> {
    use matrix_sdk_rtc_livekit::livekit::webrtc::native::yuv_helper;
    use matrix_sdk_rtc_livekit::livekit::webrtc::prelude::{I420Buffer, VideoFrame, VideoRotation};
    use v4l::buffer::Type;
    use v4l::io::traits::CaptureStream;
    use v4l::io::mmap::Stream;
    use v4l::video::Capture;

    let format = device.format().context("re-read V4L2 format")?;
    let stride = format.width as usize;
    let height = format.height as usize;
    let expected_size = stride * height + (stride * height / 2);

    let mut stream =
        Stream::with_buffers(device, Type::VideoCapture, 4).context("start V4L2 stream")?;
    let start = std::time::Instant::now();

    let mut frame = VideoFrame {
        rotation: VideoRotation::VideoRotation0,
        buffer: I420Buffer::new(resolution.width, resolution.height),
        timestamp_us: 0,
    };

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        let (data, _meta) = stream.next().context("read V4L2 frame")?;
        if data.len() < expected_size {
            warn!(
                data_len = data.len(),
                expected = expected_size,
                "V4L2 frame shorter than expected"
            );
            continue;
        }

        let (stride_y, stride_u, stride_v) = frame.buffer.strides();
        let (dst_y, dst_u, dst_v) = frame.buffer.data_mut();

        match pixel_format {
            V4l2PixelFormat::Nv12 => {
                let y_plane_len = stride * height;
                let (src_y, src_uv) = data.split_at(y_plane_len);

                yuv_helper::nv12_to_i420(
                    src_y,
                    stride as u32,
                    src_uv,
                    stride as u32,
                    dst_y,
                    stride_y,
                    dst_u,
                    stride_u,
                    dst_v,
                    stride_v,
                    resolution.width as i32,
                    resolution.height as i32,
                );
            }
            V4l2PixelFormat::Yuyv => {
                yuyv_to_i420(
                    data,
                    stride,
                    height,
                    dst_y,
                    stride_y,
                    dst_u,
                    stride_u,
                    dst_v,
                    stride_v,
                );
            }
        }

        frame.timestamp_us = start.elapsed().as_micros() as i64;
        rtc_source.capture_frame(&frame);
    }

    Ok(())
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn set_format_with_fallback(device: &mut v4l::Device, mut format: v4l::format::Format) -> anyhow::Result<v4l::format::Format> {
    use v4l::video::Capture;
    use v4l::FourCC;

    let nv12 = FourCC::new(b"NV12");
    let yuyv = FourCC::new(b"YUYV");

    format.fourcc = nv12;
    let format = device.set_format(&format).context("set V4L2 format")?;
    if format.fourcc == nv12 {
        return Ok(format);
    }

    let mut format = format;
    format.fourcc = yuyv;
    let format = device.set_format(&format).context("set V4L2 format (YUYV)")?;
    Ok(format)
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn yuyv_to_i420(
    src: &[u8],
    src_stride: usize,
    height: usize,
    dst_y: &mut [u8],
    dst_stride_y: u32,
    dst_u: &mut [u8],
    dst_stride_u: u32,
    dst_v: &mut [u8],
    dst_stride_v: u32,
) {
    let width = (src_stride / 2) as usize;
    let dst_stride_y = dst_stride_y as usize;
    let dst_stride_u = dst_stride_u as usize;
    let dst_stride_v = dst_stride_v as usize;

    for y in 0..height {
        let src_row = &src[y * src_stride..(y + 1) * src_stride];
        let dst_y_row = &mut dst_y[y * dst_stride_y..(y + 1) * dst_stride_y];
        for x in 0..width {
            let pair = x & !1;
            let base = pair * 2;
            let y_offset = if x % 2 == 0 { 0 } else { 2 };
            dst_y_row[x] = src_row[base + y_offset];
        }
    }

    let chroma_height = height / 2;
    for y in 0..chroma_height {
        let src_row0 = &src[(y * 2) * src_stride..(y * 2 + 1) * src_stride];
        let src_row1 = if y * 2 + 1 < height {
            &src[(y * 2 + 1) * src_stride..(y * 2 + 2) * src_stride]
        } else {
            src_row0
        };
        let dst_u_row = &mut dst_u[y * dst_stride_u..(y + 1) * dst_stride_u];
        let dst_v_row = &mut dst_v[y * dst_stride_v..(y + 1) * dst_stride_v];

        for x in 0..(width / 2) {
            let base = x * 4;
            let u0 = src_row0[base + 1] as u16;
            let v0 = src_row0[base + 3] as u16;
            let u1 = src_row1[base + 1] as u16;
            let v1 = src_row1[base + 3] as u16;
            dst_u_row[x] = ((u0 + u1) / 2) as u8;
            dst_v_row[x] = ((v0 + v1) / 2) as u8;
        }
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn v4l2_config_from_env() -> anyhow::Result<Option<V4l2Config>> {
    let device = match optional_env("V4L2_DEVICE") {
        Some(device) => device,
        None => return Ok(None),
    };

    let width = optional_env("V4L2_WIDTH")
        .as_deref()
        .map(str::parse::<u32>)
        .transpose()
        .context("parse V4L2_WIDTH")?;
    let height = optional_env("V4L2_HEIGHT")
        .as_deref()
        .map(str::parse::<u32>)
        .transpose()
        .context("parse V4L2_HEIGHT")?;

    Ok(Some(V4l2Config { device, width, height }))
}

#[cfg(not(all(feature = "v4l2", target_os = "linux")))]
fn v4l2_config_from_env() -> anyhow::Result<()> {
    Ok(())
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
    let v4l2_config = v4l2_config_from_env().context("read V4L2 config")?;

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

    let token_provider = EnvLiveKitTokenProvider { token: livekit_token.clone() };
    #[cfg(feature = "e2ee-per-participant")]
    let e2ee_context = build_per_participant_e2ee(&room).await?;
    #[cfg(feature = "e2ee-per-participant")]
    let room_options_provider = E2eeRoomOptionsProvider { e2ee: e2ee_context.clone() };
    #[cfg(not(feature = "e2ee-per-participant"))]
    let room_options_provider = DefaultRoomOptionsProvider;
    let connector = LiveKitSdkConnector::new(token_provider, room_options_provider);

    let service_url = ensure_access_token_query(&service_url, &livekit_token)
        .context("attach access_token to LiveKit service url")?;
    info!(
        room_id = ?room.room_id(),
        service_url = %service_url,
        token_len = livekit_token.len(),
        "starting LiveKit driver"
    );
    run_livekit_driver(
        room,
        connector,
        service_url,
        v4l2_config,
        #[cfg(feature = "e2ee-per-participant")]
        e2ee_context,
    )
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

fn ensure_access_token_query(service_url: &str, token: &str) -> anyhow::Result<String> {
    let mut url = Url::parse(service_url)?;
    let has_access_token = url
        .query_pairs()
        .any(|(key, _)| key == "access_token");
    if !has_access_token {
        url.query_pairs_mut().append_pair("access_token", token);
    }
    Ok(url.into())
}

#[cfg(feature = "e2ee-per-participant")]
async fn build_per_participant_e2ee(
    room: &matrix_sdk::Room,
) -> anyhow::Result<Option<PerParticipantE2eeContext>> {
    use matrix_sdk_rtc_livekit::matrix_keys::{
        OlmMachineKeyMaterialProvider, PerParticipantKeyMaterialProvider, room_olm_machine,
    };

    let olm_machine = match room_olm_machine(room).await {
        Ok(machine) => machine,
        Err(err) => {
            info!(?err, "no olm machine available; per-participant E2EE disabled");
            return Ok(None);
        }
    };
    let provider = OlmMachineKeyMaterialProvider::new(olm_machine);
    let bundle = provider
        .per_participant_key_bundle(room.room_id())
        .await
        .context("build per-participant key bundle")?;
    if bundle.is_empty() {
        info!("per-participant key bundle is empty; E2EE disabled");
        return Ok(None);
    }

    let bundle_bytes =
        serde_json::to_vec(&bundle).context("serialize per-participant key bundle")?;
    let digest = Sha256::digest(bundle_bytes);
    let key_provider = KeyProvider::new(KeyProviderOptions::default());
    send_per_participant_keys(room, 0, &digest).await?;

    Ok(Some(PerParticipantE2eeContext {
        key_provider,
        key_index: 0,
        local_key: digest.to_vec(),
    }))
}

#[cfg(feature = "e2ee-per-participant")]
async fn send_per_participant_keys(
    room: &matrix_sdk::Room,
    key_index: i32,
    key: &[u8],
) -> anyhow::Result<()> {
    let client = room.client();
    let own_device_id = client
        .device_id()
        .context("missing device id for per-participant E2EE")?
        .to_owned();
    let own_user_id = client.user_id().map(|id| id.to_owned());
    let members = room.members(RoomMemberships::JOIN).await?;
    let mut recipients = Vec::new();

    for member in members {
        let user_id = member.user_id();
        let devices = client.encryption().get_user_devices(user_id).await?;
        for device in devices.devices() {
            if let Some(own_user_id) = own_user_id.as_ref() {
                if device.user_id() == own_user_id && device.device_id() == &own_device_id {
                    continue;
                }
            }
            recipients.push(device);
        }
    }

    if recipients.is_empty() {
        info!("no recipient devices for per-participant E2EE to-device");
        return Ok(());
    }

    let key_b64 = STANDARD_NO_PAD.encode(key);
    let sent_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let content_raw = Raw::new(&serde_json::json!({
        "keys": [{ "index": key_index, "key": key_b64 }],
        "device_id": own_device_id.as_str(),
        "call_id": room.room_id().to_string(),
        "room_id": room.room_id().to_string(),
        "sent_ts": sent_ts,
    }))
    .context("serialize per-participant to-device payload")?
    .cast_unchecked();

    let failures = client
        .encryption()
        .encrypt_and_send_raw_to_device(
            recipients.iter().collect(),
            "io.element.call.encryption_keys",
            content_raw,
            CollectStrategy::AllDevices,
        )
        .await?;

    if failures.is_empty() {
        info!("sent per-participant E2EE keys to device recipients");
    } else {
        info!(failures = failures.len(), "failed to send per-participant E2EE keys");
    }

    Ok(())
}

async fn run_livekit_driver<O>(
    room: matrix_sdk::Room,
    connector: LiveKitSdkConnector<EnvLiveKitTokenProvider, O>,
    service_url: String,
    v4l2_config: impl V4l2ConfigOption,
    #[cfg(feature = "e2ee-per-participant")] e2ee_context: Option<PerParticipantE2eeContext>,
) -> LiveKitResult<()>
where
    O: LiveKitRoomOptionsProvider,
{
    let mut connection: Option<std::sync::Arc<Room>> = None;
    let mut info_stream = room.subscribe_info();
    #[cfg(all(feature = "v4l2", target_os = "linux"))]
    let mut v4l2_publisher: Option<V4l2CameraPublisher> = None;

    update_connection(
        &room,
        &connector,
        &service_url,
        &room.clone_info(),
        &mut connection,
        &v4l2_config,
        #[cfg(all(feature = "v4l2", target_os = "linux"))]
        &mut v4l2_publisher,
        #[cfg(feature = "e2ee-per-participant")]
        &e2ee_context,
    )
    .await?;

    while let Some(room_info) = info_stream.next().await {
        update_connection(
            &room,
            &connector,
            &service_url,
            &room_info,
            &mut connection,
            &v4l2_config,
            #[cfg(all(feature = "v4l2", target_os = "linux"))]
            &mut v4l2_publisher,
            #[cfg(feature = "e2ee-per-participant")]
            &e2ee_context,
        )
        .await?;
    }

    if connection.take().is_some() {
        #[cfg(all(feature = "v4l2", target_os = "linux"))]
        if let Some(publisher) = v4l2_publisher.take() {
            publisher
                .stop()
                .await
                .map_err(|err| LiveKitError::connector(V4l2PublishError(err)))?;
        }
    }

    Ok(())
}

async fn update_connection<O>(
    room: &matrix_sdk::Room,
    connector: &LiveKitSdkConnector<EnvLiveKitTokenProvider, O>,
    service_url: &str,
    room_info: &matrix_sdk::RoomInfo,
    connection: &mut Option<std::sync::Arc<Room>>,
    v4l2_config: &impl V4l2ConfigOption,
    #[cfg(all(feature = "v4l2", target_os = "linux"))] v4l2_publisher: &mut Option<
        V4l2CameraPublisher,
    >,
    #[cfg(feature = "e2ee-per-participant")] e2ee_context: &Option<PerParticipantE2eeContext>,
) -> LiveKitResult<()>
where
    O: LiveKitRoomOptionsProvider,
{
    let has_memberships = room_info.has_active_room_call();
    info!(
        room_id = ?room.room_id(),
        has_memberships,
        participants = room_info.active_room_call_participants().len(),
        "observed call membership state"
    );

    if has_memberships {
        if connection.is_none() {
            info!(room_id = ?room.room_id(), "joining LiveKit room for active call");
            let new_connection = connector.connect(service_url, room).await?;
            let room_handle = std::sync::Arc::new(new_connection.into_room());
            info!(
                room_name = %room_handle.name(),
                "LiveKit room connected"
            );
            #[cfg(feature = "e2ee-per-participant")]
            if let Some(context) = e2ee_context.as_ref() {
                let identity = room_handle.local_participant().identity();
                let key_set = context
                    .key_provider
                    .set_key(&identity, context.key_index, context.local_key.clone());
                room_handle.e2ee_manager().set_enabled(true);
                info!(
                    %identity,
                    key_index = context.key_index,
                    key_set,
                    "enabled per-participant E2EE for local participant"
                );
            }
            #[cfg(all(feature = "v4l2", target_os = "linux"))]
            if v4l2_publisher.is_none() {
                if let Some(config) = v4l2_config.as_option().cloned() {
                    info!(device = %config.device, "starting V4L2 camera publisher");
                    let publisher = V4l2CameraPublisher::start(
                        std::sync::Arc::clone(&room_handle),
                        config,
                    )
                    .await
                    .map_err(|err| LiveKitError::connector(V4l2PublishError(err)))?;
                    *v4l2_publisher = Some(publisher);
                }
            }
            *connection = Some(room_handle);
        }
    } else if connection.take().is_some() {
        info!(room_id = ?room.room_id(), "leaving LiveKit room because the call ended");
        #[cfg(all(feature = "v4l2", target_os = "linux"))]
        if let Some(publisher) = v4l2_publisher.take() {
            publisher
                .stop()
                .await
                .map_err(|err| LiveKitError::connector(V4l2PublishError(err)))?;
        }
    }

    Ok(())
}

trait V4l2ConfigOption {
    #[cfg(all(feature = "v4l2", target_os = "linux"))]
    fn as_option(&self) -> Option<&V4l2Config>;
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
impl V4l2ConfigOption for Option<V4l2Config> {
    fn as_option(&self) -> Option<&V4l2Config> {
        self.as_ref()
    }
}

#[cfg(not(all(feature = "v4l2", target_os = "linux")))]
impl V4l2ConfigOption for () {}
