use std::sync::Arc;
use anyhow::{anyhow, Context};
use matrix_sdk_rtc_livekit::Room as LivekitRoom;
use tracing::{info, warn};
use crate::optional_env;

#[cfg(all(feature = "v4l2", target_os = "linux"))]
#[derive(Clone, Debug)]
pub(crate) struct V4l2Config {
    pub(crate) device: String,
    pub(crate) width: Option<u32>,
    pub(crate) height: Option<u32>,
    source: V4l2VideoSource,
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
#[derive(Copy, Clone, Debug, Default)]
enum V4l2VideoSource {
    #[default]
    Camera,
    TestRedFrames,
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
#[derive(Debug)]
pub(crate) struct V4l2PublishError(pub(crate) anyhow::Error);

#[cfg(all(feature = "v4l2", target_os = "linux"))]
impl std::fmt::Display for V4l2PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
impl std::error::Error for V4l2PublishError {}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
pub(crate) struct V4l2CameraPublisher {
    room: Arc<LivekitRoom>,
    track: matrix_sdk_rtc_livekit::livekit::track::LocalVideoTrack,
    stop_tx: Option<std::sync::mpsc::Sender<()>>,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
#[derive(Copy, Clone, Debug)]
enum V4l2PixelFormat {
    Nv12,
    Yuyv,
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
impl V4l2CameraPublisher {
    pub(crate) async fn start(
        room: Arc<LivekitRoom>,
        config: V4l2Config,
    ) -> anyhow::Result<Self> {
        use matrix_sdk_rtc_livekit::livekit::options::{TrackPublishOptions, VideoCodec};
        use matrix_sdk_rtc_livekit::livekit::track::{LocalTrack, TrackSource};
        use matrix_sdk_rtc_livekit::livekit::webrtc::prelude::RtcVideoSource;

        let (resolution, rtc_source, capture_mode) =
            configure_v4l2_capture_mode(&config).context("configure V4L2 capture")?;

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
        let task = tokio::task::spawn_blocking(move || match capture_mode {
            V4l2CaptureMode::Camera { mut device, pixel_format } => run_v4l2_capture_loop(
                &mut device,
                resolution,
                pixel_format,
                rtc_source,
                stop_rx,
            ),
            V4l2CaptureMode::TestRedFrames => {
                run_generated_red_capture_loop(resolution, rtc_source, stop_rx)
            }
        });

        Ok(Self {
            room,
            track,
            stop_tx: Some(stop_tx),
            task: Some(task),
        })
    }

    pub(crate) async fn stop(&mut self) -> anyhow::Result<()> {
        info!(room_name = %self.room.name(), "stopping V4L2 camera track");
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await?;
        }
        self.room
            .local_participant()
            .unpublish_track(&self.track.sid())
            .await
            .context("unpublish V4L2 camera track")?;
        Ok(())
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
impl Drop for V4l2CameraPublisher {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }

        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
enum V4l2CaptureMode {
    Camera { device: v4l::Device, pixel_format: V4l2PixelFormat },
    TestRedFrames,
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn configure_v4l2_capture_mode(
    config: &V4l2Config,
) -> anyhow::Result<(
    matrix_sdk_rtc_livekit::livekit::webrtc::prelude::VideoResolution,
    matrix_sdk_rtc_livekit::livekit::webrtc::video_source::native::NativeVideoSource,
    V4l2CaptureMode,
)> {
    use matrix_sdk_rtc_livekit::livekit::webrtc::prelude::VideoResolution;
    use matrix_sdk_rtc_livekit::livekit::webrtc::video_source::native::NativeVideoSource;

    if matches!(config.source, V4l2VideoSource::TestRedFrames) {
        let resolution = VideoResolution {
            width: config.width.unwrap_or(640),
            height: config.height.unwrap_or(480),
        };
        let rtc_source = NativeVideoSource::new(resolution.clone(), true);
        info!(
            width = resolution.width,
            height = resolution.height,
            "configured generated red test video source"
        );
        return Ok((resolution, rtc_source, V4l2CaptureMode::TestRedFrames));
    }

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
    let rtc_source = NativeVideoSource::new(resolution.clone(), true);
    Ok((resolution, rtc_source, V4l2CaptureMode::Camera { device, pixel_format }))
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
    use v4l::io::mmap::Stream;
    use v4l::io::traits::CaptureStream;
    use v4l::video::Capture;

    let format = device.format().context("re-read V4L2 format")?;
    let width = format.width as usize;
    let height = format.height as usize;
    let stride = if format.stride == 0 {
        match pixel_format {
            V4l2PixelFormat::Nv12 => width,
            V4l2PixelFormat::Yuyv => width * 2,
        }
    } else {
        format.stride as usize
    };
    let expected_size = match pixel_format {
        V4l2PixelFormat::Nv12 => stride * height + (stride * height / 2),
        V4l2PixelFormat::Yuyv => stride * height,
    };

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
                    data, width, stride, height, dst_y, stride_y, dst_u, stride_u, dst_v, stride_v,
                );
            }
        }

        frame.timestamp_us = start.elapsed().as_micros() as i64;
        rtc_source.capture_frame(&frame);
    }

    Ok(())
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn run_generated_red_capture_loop(
    resolution: matrix_sdk_rtc_livekit::livekit::webrtc::prelude::VideoResolution,
    rtc_source: matrix_sdk_rtc_livekit::livekit::webrtc::video_source::native::NativeVideoSource,
    stop_rx: std::sync::mpsc::Receiver<()>,
) -> anyhow::Result<()> {
    use matrix_sdk_rtc_livekit::livekit::webrtc::prelude::{I420Buffer, VideoFrame, VideoRotation};

    let mut frame = VideoFrame {
        rotation: VideoRotation::VideoRotation0,
        buffer: I420Buffer::new(resolution.width, resolution.height),
        timestamp_us: 0,
    };
    let (stride_y, stride_u, stride_v) = frame.buffer.strides();
    let (dst_y, dst_u, dst_v) = frame.buffer.data_mut();

    fill_plane(dst_y, stride_y as usize, resolution.width as usize, resolution.height as usize, 76);
    fill_plane(
        dst_u,
        stride_u as usize,
        (resolution.width / 2) as usize,
        (resolution.height / 2) as usize,
        84,
    );
    fill_plane(
        dst_v,
        stride_v as usize,
        (resolution.width / 2) as usize,
        (resolution.height / 2) as usize,
        255,
    );

    let frame_duration = std::time::Duration::from_millis(33);
    let start = std::time::Instant::now();
    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        frame.timestamp_us = start.elapsed().as_micros() as i64;
        rtc_source.capture_frame(&frame);
        std::thread::sleep(frame_duration);
    }

    Ok(())
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn fill_plane(dst: &mut [u8], stride: usize, width: usize, height: usize, value: u8) {
    for y in 0..height {
        let row = &mut dst[y * stride..y * stride + width];
        row.fill(value);
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn set_format_with_fallback(
    device: &mut v4l::Device,
    mut format: v4l::format::Format,
) -> anyhow::Result<v4l::format::Format> {
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
    width: usize,
    src_stride: usize,
    height: usize,
    dst_y: &mut [u8],
    dst_stride_y: u32,
    dst_u: &mut [u8],
    dst_stride_u: u32,
    dst_v: &mut [u8],
    dst_stride_v: u32,
) {
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
pub(crate) fn v4l2_config_from_env() -> anyhow::Result<Option<V4l2Config>> {
    let source = match optional_env("V4L2_VIDEO_SOURCE")
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("camera") | Some("webcam") | None => V4l2VideoSource::Camera,
        Some("test_red") | Some("test-red") | Some("red") => V4l2VideoSource::TestRedFrames,
        Some(other) => {
            return Err(anyhow!(
                "invalid V4L2_VIDEO_SOURCE '{other}'; expected camera|webcam|test_red|test-red|red"
            ));
        }
    };

    let device = if matches!(source, V4l2VideoSource::Camera) {
        match optional_env("V4L2_DEVICE") {
            Some(device) => device,
            None => return Ok(None),
        }
    } else {
        optional_env("V4L2_DEVICE").unwrap_or_else(|| "generated-test-source".to_owned())
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

    Ok(Some(V4l2Config { device, width, height, source }))
}

#[cfg(not(all(feature = "v4l2", target_os = "linux")))]
pub(crate) fn v4l2_config_from_env() -> anyhow::Result<()> {
    Ok(())
}
