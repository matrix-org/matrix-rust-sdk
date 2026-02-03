#![recursion_limit = "256"]

use std::{env, fs};

use anyhow::{Context, anyhow};
use matrix_sdk::{
    Client,
    event_handler::EventHandlerDropGuard,
    RoomState,
    RoomMemberships,
    config::SyncSettings,
    ruma::{OwnedRoomId, OwnedServerName, RoomId, RoomOrAliasId, ServerName},
};
#[cfg(feature = "e2e-encryption")]
use matrix_sdk::encryption::secret_storage::SecretStore;
#[cfg(feature = "experimental-widgets")]
use matrix_sdk::{
    ruma::{DeviceId, UserId},
    widget::{
        Capabilities, CapabilitiesProvider, ClientProperties, EncryptionSystem, Filter, Intent,
        MessageLikeEventFilter, StateEventFilter, ToDeviceEventFilter,
        VirtualElementCallWidgetConfig, VirtualElementCallWidgetProperties, WidgetDriver,
        WidgetSettings,
    },
};
#[cfg(feature = "experimental-widgets")]
use ruma::events::{
    TimelineEventType,
    call::member::{
        ActiveFocus, ActiveLivekitFocus, Application, CallApplicationContent,
        CallMemberEventContent, CallMemberStateKey, CallScope, Focus, LivekitFocus,
    },
};
use matrix_sdk_rtc::{LiveKitConnector, LiveKitResult, livekit_service_url};
#[cfg(all(feature = "v4l2", target_os = "linux"))]
use matrix_sdk_rtc::LiveKitError;
use matrix_sdk_rtc_livekit::{
    LiveKitRoomOptionsProvider, LiveKitSdkConnector, LiveKitTokenProvider, RoomOptions, Room,
};
#[cfg(feature = "e2e-encryption")]
use futures_util::StreamExt;
#[cfg(feature = "e2ee-per-participant")]
use matrix_sdk_rtc_livekit::livekit::e2ee::{
    E2eeOptions, EncryptionType,
    key_provider::{KeyProvider, KeyProviderOptions},
};
#[cfg(feature = "e2ee-per-participant")]
use matrix_sdk_rtc_livekit::livekit::id::ParticipantIdentity;
#[cfg(feature = "e2ee-per-participant")]
use matrix_sdk::ruma::CanonicalJsonValue;
#[cfg(feature = "e2ee-per-participant")]
use matrix_sdk_crypto::types::room_history::RoomKeyBundle;
use ruma::api::client::account::request_openid_token;
#[cfg(feature = "e2ee-per-participant")]
use ruma::serde::Raw;
#[cfg(feature = "e2ee-per-participant")]
use ruma::events::AnyToDeviceEvent;
use serde_json::Value as JsonValue;
#[cfg(feature = "e2ee-per-participant")]
use base64::{Engine as _, engine::general_purpose::{STANDARD, STANDARD_NO_PAD}};
#[cfg(feature = "e2ee-per-participant")]
use matrix_sdk_base::crypto::CollectStrategy;
#[cfg(feature = "e2ee-per-participant")]
use sha2::{Digest, Sha256};
#[cfg(feature = "experimental-widgets")]
use tokio::io::{AsyncBufReadExt, BufReader};
#[cfg(feature = "experimental-widgets")]
use tokio::sync::watch;
use tracing::info;
#[cfg(all(feature = "v4l2", target_os = "linux"))]
use tracing::warn;
use url::Url;

struct EnvLiveKitTokenProvider {
    token: String,
}

struct DefaultRoomOptionsProvider;

#[cfg(feature = "experimental-widgets")]
#[derive(Clone)]
struct StaticCapabilitiesProvider {
    capabilities: Capabilities,
}

#[cfg(feature = "experimental-widgets")]
#[derive(Clone)]
struct ElementCallWidget {
    handle: matrix_sdk::widget::WidgetDriverHandle,
    widget_id: String,
    capabilities_ready: watch::Receiver<bool>,
}

#[cfg(not(feature = "experimental-widgets"))]
struct ElementCallWidget;

#[cfg(feature = "experimental-widgets")]
impl CapabilitiesProvider for StaticCapabilitiesProvider {
    async fn acquire_capabilities(&self, _capabilities: Capabilities) -> Capabilities {
        self.capabilities.clone()
    }
}

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
        use matrix_sdk_rtc_livekit::livekit::track::{LocalTrack, TrackSource};
        use matrix_sdk_rtc_livekit::livekit::webrtc::prelude::RtcVideoSource;

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
    use v4l::Device;

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
    let width = src_stride / 2;
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
    let device_id = optional_env("MATRIX_DEVICE_ID");
    let livekit_service_url_override = optional_env("LIVEKIT_SERVICE_URL");
    let livekit_sfu_get_url = optional_env("LIVEKIT_SFU_GET_URL");
    let v4l2_config = v4l2_config_from_env().context("read V4L2 config")?;

    let store_dir = std::env::current_dir()
        .context("read current directory")?
        .join("matrix-sdk-store");
    fs::create_dir_all(&store_dir).context("create crypto store directory")?;
    let store_path = store_dir.join("matrix-sdk.sqlite");
    if store_path.is_dir() {
        warn!(
            store_path = %store_path.display(),
            "Removing directory that conflicts with sqlite store file path."
        );
        fs::remove_dir_all(&store_path).context("remove sqlite store directory")?;
    }

    let mut client_builder = Client::builder().homeserver_url(homeserver_url);

    #[cfg(feature = "sqlite")]
    {
        client_builder = client_builder.sqlite_store(store_path, None);
    }
    #[cfg(not(feature = "sqlite"))]
    {
        let _ = &store_path;
        warn!("sqlite feature disabled; crypto store will be in-memory.");
    }

    let client = client_builder.build().await.context("build Matrix client")?;

    let mut login_builder = client.matrix_auth().login_username(&username, &password);
    if let Some(device_id) = device_id.as_deref() {
        login_builder = login_builder.device_id(device_id);
    }
    login_builder.send().await.context("login Matrix user")?;
    import_recovery_key_if_set(&client)
        .await
        .context("import recovery key")?;
    log_backup_state(&client).await;

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
    spawn_backup_diagnostics(client.clone(), room.room_id().to_owned());
    let element_call_url = optional_env("ELEMENT_CALL_URL")
        .or_else(|| optional_env("ELEMENT_CALL_WIDGET"));
    let widget = if let Some(element_call_url) = element_call_url {
        info!(%element_call_url, "Element Call widget URL set; starting widget bridge");
        start_element_call_widget(room.clone(), element_call_url)
            .await
            .context("start element call widget")?
    } else if optional_env("ELEMENT_CALL_WIDGET_ID").is_some() {
        info!("ELEMENT_CALL_WIDGET_ID set but no Element Call URL provided");
        None
    } else {
        None
    };
    #[cfg(not(feature = "experimental-widgets"))]
    let _ = &widget;

    let sync_client = client.clone();
    let sync_handle = tokio::spawn(async move {
        sync_client.sync(SyncSettings::new()).await
    });

    // NOTE: Joining a call requires publishing MatrixRTC memberships (m.call.member) for
    // this device. When the optional Element Call widget wiring is enabled, this example
    // publishes a membership via the widget API before starting the driver so the
    // membership is visible to other clients.
    //
    // The optional Element Call widget wiring is how a Rust client can integrate
    // with the Element Call webapp:
    // - The widget driver bridges postMessage traffic to/from the webview/iframe.
    // - Capabilities allow Element Call to send/receive to-device encryption keys
    //   (io.element.call.encryption_keys), which the Rust SDK consumes for per-participant
    //   E2EE when enabled.
    // - When running Element Call inside element-web directly (not embedded), the widget
    //   bridge logs below will not appear because the Rust SDK is not connected to that
    //   webview's postMessage channel.

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

    #[cfg(feature = "experimental-widgets")]
    if let Some(widget) = widget.as_ref() {
        publish_call_membership_via_widget(room.clone(), widget, &service_url)
            .await
            .context("publish MatrixRTC membership via widget api")?;
    }

    let token_provider = EnvLiveKitTokenProvider { token: livekit_token.clone() };
    #[cfg(feature = "e2ee-per-participant")]
    let e2ee_context = build_per_participant_e2ee(&room).await?;
    #[cfg(feature = "e2ee-per-participant")]
    if let Some(context) = e2ee_context.as_ref() {
        spawn_periodic_e2ee_key_resend(room.clone(), context.clone());
    }
    #[cfg(feature = "e2ee-per-participant")]
    let _e2ee_to_device_guard = e2ee_context
        .as_ref()
        .map(|context| register_e2ee_to_device_handler(&client, room.room_id().to_owned(), context.key_provider.clone()));
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

fn bool_env(name: &str) -> bool {
    optional_env(name).is_some_and(|value| {
        matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

fn retry_attempts_from_env(name: &str, default: usize) -> usize {
    optional_env(name)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
}

fn retry_seconds_from_env(name: &str, default: u64) -> u64 {
    optional_env(name)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(default)
}

#[cfg(feature = "e2e-encryption")]
async fn import_recovery_key_if_set(client: &Client) -> anyhow::Result<()> {
    let Some(recovery_key) = optional_env("MATRIX_RECOVERY_KEY") else {
        return Ok(());
    };
    if recovery_key.trim_start().starts_with('{') {
        info!(
            "MATRIX_RECOVERY_KEY looks like JSON; provide the secret storage recovery key string instead"
        );
    }
    info!("MATRIX_RECOVERY_KEY set; attempting to import secrets from secret storage");
    let secret_store: SecretStore = client
        .encryption()
        .secret_storage()
        .open_secret_store(&recovery_key)
        .await
        .context("open secret storage with recovery key")?;
    secret_store
        .import_secrets()
        .await
        .context("import secrets from secret storage")?;
    info!("recovery key import finished");
    Ok(())
}

#[cfg(feature = "e2e-encryption")]
async fn log_backup_state(client: &Client) {
    let backups = client.encryption().backups();
    let state = backups.state();
    let enabled = backups.are_enabled().await;
    let exists = backups.fetch_exists_on_server().await.unwrap_or(false);
    info!(
        ?state,
        enabled,
        exists_on_server = exists,
        "backup state summary"
    );
    if exists && !enabled {
        info!(
            "backup exists on the server but backups are not enabled; ensure the recovery key is available"
        );
    }
}

#[cfg(not(feature = "e2e-encryption"))]
async fn import_recovery_key_if_set(_client: &Client) -> anyhow::Result<()> {
    if optional_env("MATRIX_RECOVERY_KEY").is_some() {
        info!(
            "MATRIX_RECOVERY_KEY set but e2e-encryption feature is disabled; enable the example's e2e-encryption feature"
        );
    }
    Ok(())
}

#[cfg(not(feature = "e2e-encryption"))]
async fn log_backup_state(_client: &Client) {}

#[cfg(feature = "e2e-encryption")]
fn spawn_backup_diagnostics(client: Client, room_id: OwnedRoomId) {
    let backup_client = client.clone();
    tokio::spawn(async move {
        let mut state_stream = backup_client.encryption().backups().state_stream();
        while let Some(update) = state_stream.next().await {
            match update {
                Ok(state) => info!(?state, "backup state updated"),
                Err(err) => info!(?err, "backup state stream error"),
            }
        }
        info!("backup state stream closed");
    });

    tokio::spawn(async move {
        let key_stream = client
            .encryption()
            .backups()
            .room_keys_for_room_stream(&room_id);
        futures_util::pin_mut!(key_stream);
        while let Some(update) = key_stream.next().await {
            match update {
                Ok(room_keys) => info!(?room_keys, "received room keys from backup"),
                Err(err) => info!(?err, "room key backup stream error"),
            }
        }
        info!("room key backup stream closed");
    });
}

#[cfg(not(feature = "e2e-encryption"))]
fn spawn_backup_diagnostics(_client: Client, _room_id: OwnedRoomId) {}

#[cfg(feature = "e2ee-per-participant")]
fn spawn_periodic_e2ee_key_resend(
    room: matrix_sdk::Room,
    context: PerParticipantE2eeContext,
) {
    let interval_secs = retry_seconds_from_env("PER_PARTICIPANT_KEY_RESEND_SECS", 0);
    if interval_secs == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            interval.tick().await;
            info!(
                interval_secs,
                key_index = context.key_index,
                "periodic per-participant E2EE key resend"
            );
            if let Err(err) =
                send_per_participant_keys(&room, context.key_index, &context.local_key).await
            {
                info!(?err, "failed to resend per-participant E2EE keys");
            }
        }
    });
}

#[cfg(not(feature = "e2ee-per-participant"))]
fn spawn_periodic_e2ee_key_resend(_room: matrix_sdk::Room, _context: ()) {}

#[cfg(feature = "experimental-widgets")]
fn element_call_capabilities(own_user_id: &UserId, own_device_id: &DeviceId) -> Capabilities {
    use ruma::events::{MessageLikeEventType, StateEventType};

    let read_send = vec![
        Filter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::from(
            "org.matrix.rageshake_request",
        ))),
        Filter::ToDevice(ToDeviceEventFilter::new(
            "io.element.call.encryption_keys".into(),
        )),
        Filter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::from(
            "io.element.call.encryption_keys",
        ))),
        Filter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::from(
            "io.element.call.reaction",
        ))),
        Filter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::Reaction)),
        Filter::MessageLike(MessageLikeEventFilter::WithType(
            MessageLikeEventType::RoomRedaction,
        )),
        Filter::MessageLike(MessageLikeEventFilter::WithType(
            MessageLikeEventType::RtcDecline,
        )),
    ];

    let user_id = own_user_id.as_str();
    let device_id = own_device_id.as_str();
    let membership_state_key = CallMemberStateKey::new(
        own_user_id.to_owned(),
        Some(format!("{own_device_id}_m.call")),
        false,
    )
    .as_ref()
    .to_owned();

    Capabilities {
        read: vec![
            Filter::State(StateEventFilter::WithType(StateEventType::CallMember)),
            Filter::State(StateEventFilter::WithType(StateEventType::RoomName)),
            Filter::State(StateEventFilter::WithType(StateEventType::RoomMember)),
            Filter::State(StateEventFilter::WithType(StateEventType::RoomEncryption)),
            Filter::State(StateEventFilter::WithType(StateEventType::RoomCreate)),
        ]
        .into_iter()
        .chain(read_send.clone())
        .collect(),
        send: vec![
            Filter::MessageLike(MessageLikeEventFilter::WithType(
                MessageLikeEventType::RtcNotification,
            )),
            Filter::MessageLike(MessageLikeEventFilter::WithType(
                MessageLikeEventType::CallNotify,
            )),
            Filter::State(StateEventFilter::WithTypeAndStateKey(
                StateEventType::CallMember,
                user_id.to_owned(),
            )),
            Filter::State(StateEventFilter::WithTypeAndStateKey(
                StateEventType::CallMember,
                format!("{user_id}_{device_id}"),
            )),
            Filter::State(StateEventFilter::WithTypeAndStateKey(
                StateEventType::CallMember,
                membership_state_key,
            )),
            Filter::State(StateEventFilter::WithTypeAndStateKey(
                StateEventType::CallMember,
                format!("{user_id}_{device_id}_m.call"),
            )),
            Filter::State(StateEventFilter::WithTypeAndStateKey(
                StateEventType::CallMember,
                format!("_{user_id}_{device_id}"),
            )),
            Filter::State(StateEventFilter::WithTypeAndStateKey(
                StateEventType::CallMember,
                format!("_{user_id}_{device_id}_m.call"),
            )),
        ]
        .into_iter()
        .chain(read_send)
        .collect(),
        requires_client: true,
        update_delayed_event: true,
        send_delayed_event: true,
    }
}

#[cfg(feature = "experimental-widgets")]
async fn start_element_call_widget(
    room: matrix_sdk::Room,
    element_call_url: String,
) -> anyhow::Result<Option<ElementCallWidget>> {
    let own_user_id = room
        .client()
        .user_id()
        .context("missing user id for element call widget")?
        .to_owned();
    let own_device_id = room
        .client()
        .device_id()
        .context("missing device id for element call widget")?
        .to_owned();

    let encryption_state = room
        .latest_encryption_state()
        .await
        .context("load room encryption state for element call")?;
    let encryption = if encryption_state.is_encrypted() {
        info!("room is encrypted; Element Call will be configured for E2EE");
        #[cfg(feature = "e2ee-per-participant")]
        {
            EncryptionSystem::PerParticipantKeys
        }
        #[cfg(not(feature = "e2ee-per-participant"))]
        {
            info!("room is encrypted but per-participant E2EE is disabled at compile time");
            EncryptionSystem::Unencrypted
        }
    } else {
        info!("room is not encrypted; Element Call will be configured unencrypted");
        EncryptionSystem::Unencrypted
    };

    let widget_id = optional_env("ELEMENT_CALL_WIDGET_ID")
        .unwrap_or_else(|| "element-call".to_owned());
    let props = VirtualElementCallWidgetProperties {
        element_call_url,
        widget_id,
        parent_url: optional_env("ELEMENT_CALL_PARENT_URL"),
        encryption,
        ..Default::default()
    };
    let config = VirtualElementCallWidgetConfig {
        intent: Some(Intent::JoinExisting),
        ..Default::default()
    };

    let widget_settings = WidgetSettings::new_virtual_element_call_widget(props, config)
        .context("build element call widget settings")?;
    let widget_id = widget_settings.widget_id().to_owned();
    let widget_url = widget_settings
        .generate_webview_url(
            &room,
            ClientProperties::new("matrix-sdk-rtc-livekit-join", None, None),
        )
        .await
        .context("generate element call widget url")?;
    info!(%widget_url, "element call widget url");
    info!(
        widget_id = %widget_settings.widget_id(),
        "starting Element Call widget driver"
    );

    let (driver, handle) = WidgetDriver::new(widget_settings);
    let capabilities = element_call_capabilities(&own_user_id, &own_device_id);
    let capabilities_provider = StaticCapabilitiesProvider { capabilities };
    let widget_capabilities = capabilities_provider.capabilities.clone();
    let (capabilities_ready_tx, capabilities_ready_rx) = watch::channel(false);
    tokio::spawn(async move {
        if driver.run(room, capabilities_provider).await.is_err() {
            info!("element call widget driver stopped");
        }
    });

    let outbound_handle = handle.clone();
    let outbound_widget_id = widget_id.clone();
    tokio::spawn(async move {
        let capabilities_ready_tx = capabilities_ready_tx;
        while let Some(message) = outbound_handle.recv().await {
            info!("widget -> rust-sdk message forwarded to stdout");
            println!("{message}");
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&message) else {
                continue;
            };
            let Some(action) = value.get("action").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(request_id) = value.get("requestId").and_then(|v| v.as_str()) else {
                continue;
            };
            if action == "capabilities" {
                let response = serde_json::json!({
                    "api": "toWidget",
                    "widgetId": outbound_widget_id,
                    "requestId": request_id,
                    "action": "capabilities",
                    "data": {},
                    "response": {
                        "capabilities": widget_capabilities,
                    },
                });
                if !outbound_handle.send(response.to_string()).await {
                    break;
                }
            }
            if action == "notify_capabilities" {
                let response = serde_json::json!({
                    "api": "toWidget",
                    "widgetId": outbound_widget_id,
                    "requestId": request_id,
                    "action": "notify_capabilities",
                    "data": {},
                    "response": {},
                });
                if !outbound_handle.send(response.to_string()).await {
                    break;
                }
                let _ = capabilities_ready_tx.send(true);
            }
        }
        info!("widget -> rust-sdk message stream closed");
    });

    let inbound_handle = handle.clone();
    tokio::spawn(async move {
        let stdin = BufReader::new(tokio::io::stdin());
        let mut lines = stdin.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if !inbound_handle.send(line).await {
                break;
            }
            info!("stdin -> widget message forwarded");
        }
        info!("stdin -> widget message stream closed");
    });

    let content_loaded = serde_json::json!({
        "api": "fromWidget",
        "widgetId": widget_id,
        "requestId": format!("content-loaded-{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()),
        "action": "content_loaded",
        "data": {},
    });
    let _ = handle.send(content_loaded.to_string()).await;

    Ok(Some(ElementCallWidget {
        handle,
        widget_id,
        capabilities_ready: capabilities_ready_rx,
    }))
}

#[cfg(not(feature = "experimental-widgets"))]
async fn start_element_call_widget(
    _room: matrix_sdk::Room,
    _element_call_url: String,
) -> anyhow::Result<Option<ElementCallWidget>> {
    info!("ELEMENT_CALL_URL set but experimental-widgets feature is disabled");
    Ok(None)
}

#[cfg(feature = "experimental-widgets")]
async fn publish_call_membership_via_widget(
    room: matrix_sdk::Room,
    widget: &ElementCallWidget,
    service_url: &str,
) -> anyhow::Result<()> {
    if !*widget.capabilities_ready.borrow() {
        let mut capabilities_ready = widget.capabilities_ready.clone();
        let _ = capabilities_ready.changed().await;
    }
    let own_user_id = room
        .client()
        .user_id()
        .context("missing user id for widget membership publisher")?
        .to_owned();
    let own_device_id = room
        .client()
        .device_id()
        .context("missing device id for widget membership publisher")?
        .to_owned();
    let state_key = CallMemberStateKey::new(own_user_id.clone(), None, false);
    let call_id = room.room_id().to_string();
    let application = Application::Call(CallApplicationContent::new(call_id.clone(), CallScope::Room));
    let focus_active = ActiveFocus::Livekit(ActiveLivekitFocus::new());
    let focus_alias = format!("livekit-{}", room.room_id());
    let foci_preferred = vec![Focus::Livekit(LivekitFocus::new(
        focus_alias,
        service_url.to_owned(),
    ))];
    let content = CallMemberEventContent::new(
        application,
        own_device_id,
        focus_active,
        foci_preferred,
        None,
        None,
    );
    let request_id = format!(
        "publish-membership-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );
    let message = serde_json::json!({
        "api": "fromWidget",
        "widgetId": widget.widget_id,
        "requestId": request_id,
        "action": "send_event",
        "data": {
            "type": TimelineEventType::CallMember.to_string(),
            "room_id": room.room_id().to_string(),
            "state_key": state_key.as_ref(),
            "content": content,
        }
    });
    if !widget.handle.send(message.to_string()).await {
        return Err(anyhow!("widget driver handle closed before sending membership"));
    }
    info!("published MatrixRTC membership via widget api");
    Ok(())
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
    room_id: OwnedRoomId,
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

    let encryption_state = room
        .latest_encryption_state()
        .await
        .context("load room encryption state")?;
    if !encryption_state.is_encrypted() {
        info!("room is not encrypted; per-participant E2EE disabled");
        return Ok(None);
    }
    info!("room encryption enabled; attempting per-participant E2EE setup");

    let force_download = bool_env("PER_PARTICIPANT_FORCE_BACKUP_DOWNLOAD");
    if force_download {
        let backups = room.client().encryption().backups();
        match backups.download_room_keys_for_room(room.room_id()).await {
            Ok(()) => info!("requested room keys from backup for per-participant E2EE"),
            Err(err) => info!(?err, "failed to request room keys from backup"),
        }
    }

    let olm_machine = match room_olm_machine(room).await {
        Ok(machine) => machine,
        Err(err) => {
            info!(?err, "no olm machine available; per-participant E2EE disabled");
            return Ok(None);
        }
    };
    let provider = OlmMachineKeyMaterialProvider::new(olm_machine);
    let retries = retry_attempts_from_env("PER_PARTICIPANT_KEY_RETRIES", 0);
    let mut attempt = 0usize;
    let bundle = loop {
        let bundle = provider
            .per_participant_key_bundle(room.room_id())
            .await
            .context("build per-participant key bundle")?;
        if !bundle.is_empty() {
            break bundle;
        }
        if attempt >= retries {
            info!(
                retries,
                "per-participant key bundle is empty; E2EE disabled"
            );
            return Ok(None);
        }
        attempt += 1;
        if force_download {
            let backups = room.client().encryption().backups();
            match backups.download_room_keys_for_room(room.room_id()).await {
                Ok(()) => info!("requested room keys from backup for per-participant E2EE"),
                Err(err) => info!(?err, "failed to request room keys from backup"),
            }
        }
        info!(
            attempt,
            retries,
            "per-participant key bundle empty; retrying after delay"
        );
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    };
    info!(
        room_keys = bundle.room_keys.len(),
        withheld_keys = bundle.withheld.len(),
        "per-participant key bundle details"
    );
    info!("per-participant key bundle available; enabling E2EE");
    let key_provider = KeyProvider::new(KeyProviderOptions::default());
    let local_key = derive_per_participant_key(&bundle)?;
    send_per_participant_keys(room, 0, &local_key).await?;

    Ok(Some(PerParticipantE2eeContext {
        key_provider,
        key_index: 0,
        local_key,
    }))
}

#[cfg(feature = "e2ee-per-participant")]
fn derive_per_participant_key(bundle: &RoomKeyBundle) -> anyhow::Result<Vec<u8>> {
    let room_keys = canonicalize_bundle_entries(&bundle.room_keys)
        .context("canonicalize per-participant room keys")?;
    let withheld =
        canonicalize_bundle_entries(&bundle.withheld).context("canonicalize withheld keys")?;
    let bundle_value = serde_json::json!({
        "room_keys": room_keys,
        "withheld": withheld,
    });
    let canonical: CanonicalJsonValue = bundle_value
        .try_into()
        .context("canonicalize per-participant key bundle")?;
    let canonical_bytes = serde_json::to_vec(&canonical)
        .context("serialize canonical per-participant key bundle")?;
    let digest = Sha256::digest(canonical_bytes);

    Ok(digest.to_vec())
}

#[cfg(feature = "e2ee-per-participant")]
fn canonicalize_bundle_entries<T: serde::Serialize>(
    entries: &[T],
) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut canonical_entries = Vec::with_capacity(entries.len());

    for entry in entries {
        let value = serde_json::to_value(entry).context("serialize bundle entry")?;
        let canonical: CanonicalJsonValue = value
            .clone()
            .try_into()
            .context("canonicalize bundle entry")?;
        let sort_key = serde_json::to_string(&canonical).context("serialize bundle entry")?;
        canonical_entries.push((sort_key, value));
    }

    canonical_entries.sort_by(|left, right| left.0.cmp(&right.0));

    Ok(canonical_entries
        .into_iter()
        .map(|(_, value)| value)
        .collect())
}

#[cfg(feature = "e2ee-per-participant")]
async fn send_per_participant_keys(
    room: &matrix_sdk::Room,
    key_index: i32,
    key: &[u8],
) -> anyhow::Result<()> {
    if key.is_empty() {
        info!(
            key_index,
            "per-participant E2EE key payload is empty; skipping send"
        );
        return Ok(());
    }
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
        let device_list: Vec<_> = devices.devices().collect();
        info!(
            user_id = %user_id,
            device_count = device_list.len(),
            "per-participant E2EE device discovery"
        );
        for device in device_list {
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
    info!(
        recipients = recipients.len(),
        key_index,
        room_id = %room.room_id(),
        "sending per-participant E2EE keys to devices"
    );

    let key_b64 = STANDARD_NO_PAD.encode(key);
    let sent_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    info!(
        key_index,
        key_len = key.len(),
        "preparing per-participant E2EE key payload"
    );
    let content_raw = Raw::new(&serde_json::json!({
        "keys": [{ "index": key_index, "key": key_b64 }],
        "device_id": own_device_id.as_str(),
        "call_id": "",
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

#[cfg(feature = "e2ee-per-participant")]
fn register_e2ee_to_device_handler(
    client: &Client,
    room_id: OwnedRoomId,
    key_provider: KeyProvider,
) -> EventHandlerDropGuard {
    let handle = client.add_event_handler(move |raw: Raw<AnyToDeviceEvent>| {
        let key_provider = key_provider.clone();
        let room_id = room_id.clone();
        async move {
            let Ok(value) = raw.deserialize_as::<serde_json::Value>() else {
                return;
            };
            let Some(event_type) = value.get("type").and_then(|v| v.as_str()) else {
                return;
            };
            if event_type != "io.element.call.encryption_keys" {
                return;
            }

            let Some(sender) = value.get("sender").and_then(|v| v.as_str()) else {
                return;
            };
            let Some(content) = value.get("content").and_then(|v| v.as_object()) else {
                return;
            };
            let Some(content_room_id) = content.get("room_id").and_then(|v| v.as_str()) else {
                return;
            };
            if content_room_id != room_id.as_str() {
                return;
            }
            let Some(device_id) = content.get("device_id").and_then(|v| v.as_str()) else {
                return;
            };
            let Some(keys) = content.get("keys").and_then(|v| v.as_array()) else {
                return;
            };

            let identity = ParticipantIdentity(format!("{sender}:{device_id}"));
            for key_entry in keys {
                let Some(index) = key_entry.get("index").and_then(|v| v.as_i64()) else {
                    continue;
                };
                let Some(key_b64) = key_entry.get("key").and_then(|v| v.as_str()) else {
                    continue;
                };
                let key_bytes = STANDARD_NO_PAD
                    .decode(key_b64)
                    .or_else(|_| STANDARD.decode(key_b64));
                let Ok(key_bytes) = key_bytes else {
                    continue;
                };
                let key_set = key_provider.set_key(&identity, index as i32, key_bytes);
                info!(
                    %identity,
                    key_index = index,
                    key_set,
                    "applied per-participant E2EE key from to-device"
                );
            }
        }
    });

    client.event_handler_drop_guard(handle)
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
