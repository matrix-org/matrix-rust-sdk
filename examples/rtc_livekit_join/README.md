# LiveKit call join (skeleton)

This example shows how to wire `matrix-sdk-rtc` and `matrix-sdk-rtc-livekit` to
join/leave a LiveKit room based on MatrixRTC call memberships.

**Important:** this example needs MatrixRTC memberships (e.g. `m.call.member`)
for your device before the `LiveKitRoomDriver` will connect. You can either
publish memberships yourself or enable the optional Element Call widget bridge
feature (see below), which publishes the membership via the widget API.

## Usage

```bash
HOMESERVER_URL=https://matrix.example.org \
MATRIX_USERNAME=@alice:example.org \
MATRIX_PASSWORD=secret \
ROOM_ID=!roomid:example.org \
VIA_SERVERS=example.org,otherserver.org \
LIVEKIT_SFU_GET_URL=https://demo.call.bundesmessenger.info/sfu/get \
LIVEKIT_SERVICE_URL=wss://livekit.example.org \
LIVEKIT_TOKEN=your-token \
RUST_LOG=info \
cargo run -p example-rtc-livekit-join
```

### Element Call widget bridge (membership publisher)

To publish the call membership via the Element Call widget API, enable the
`experimental-widgets` feature and provide the widget URL:

```bash
HOMESERVER_URL=https://matrix.example.org \
MATRIX_USERNAME=@alice:example.org \
MATRIX_PASSWORD=secret \
ROOM_ID=!roomid:example.org \
ELEMENT_CALL_URL=https://call.element.io \
RUST_LOG=info \
cargo run -p example-rtc-livekit-join --features experimental-widgets
```

## V4L2 camera publishing (Linux)

This example can publish a local V4L2 camera (e.g. `/dev/video0`) into the
LiveKit room using the LiveKit Rust SDK. Enable the feature and provide the
device path:

```bash
HOMESERVER_URL=https://matrix.example.org \
MATRIX_USERNAME=@alice:example.org \
MATRIX_PASSWORD=secret \
ROOM_ID=!roomid:example.org \
LIVEKIT_SERVICE_URL=wss://livekit.example.org \
LIVEKIT_TOKEN=your-token \
V4L2_DEVICE=/dev/video0 \
V4L2_VIDEO_SOURCE=camera \
V4L2_WIDTH=1280 \
V4L2_HEIGHT=720 \
RUST_LOG=info \
cargo run -p example-rtc-livekit-join --features v4l2
```


To publish generated solid red test frames instead of a real camera stream, set
`V4L2_VIDEO_SOURCE=test_red` (or `test-red` / `red`). In this mode, `V4L2_DEVICE`
is optional and defaults to 640x480 when width/height are omitted:

```bash
HOMESERVER_URL=https://matrix.example.org \
MATRIX_USERNAME=@alice:example.org \
MATRIX_PASSWORD=secret \
ROOM_ID=!roomid:example.org \
LIVEKIT_SERVICE_URL=wss://livekit.example.org \
LIVEKIT_TOKEN=your-token \
V4L2_VIDEO_SOURCE=test_red \
V4L2_WIDTH=1280 \
V4L2_HEIGHT=720 \
RUST_LOG=info \
cargo run -p example-rtc-livekit-join --features v4l2
```

To enable per-participant E2EE (MSC4268 key bundles), add the
`e2ee-per-participant` feature:

```bash
cargo run -p example-rtc-livekit-join --features v4l2,e2ee-per-participant
```

Notes:

- The capture path prefers NV12, but will fall back to YUYV if NV12 is not
  supported. If your camera defaults to MJPEG or another format, use `v4l2-ctl`
  to switch it:

  ```bash
  v4l2-ctl -d /dev/video0 --set-fmt-video=width=1280,height=720,pixelformat=NV12
  ```

- `V4L2_VIDEO_SOURCE` selects the published source: `camera`/`webcam` (default)
  or `test_red`/`test-red`/`red` for generated red frames.
- `V4L2_WIDTH` and `V4L2_HEIGHT` are optional; for camera mode, the current
  device format is used when omitted. For generated red frames, they default to
  640x480.

## Build notes (Linux)

The LiveKit/WebRTC dependency currently links against libstdc++. If you build
with `clang`/`libc++`, you may see undefined references like
`std::__throw_bad_array_new_length()` or `std::__glibcxx_assert_fail`. In that
case, build with the default GNU toolchain or ensure libstdc++ is linked.
For example:

```bash
cargo build -p example-rtc-livekit-join
```

If you still see C++ compilation failures such as `changes meaning of 'Network'
[-fpermissive]` or `-Wno-changes-meaning` being rejected, ensure both `CC` and
`CXX` point to the GNU toolchain (`gcc`/`g++`). The bundled WebRTC headers expect
GCC-style diagnostics and may not compile with `clang` in some environments.

Also avoid mixing GNU `g++` with libc++ headers (for example, `CXX=g++` combined
with `-isystem /usr/include/c++/v1`), which can trigger template errors inside
`cxx` and the standard library. Prefer the default libstdc++ headers when using
`g++` (i.e., drop the libc++ include path and let `g++` pick its own headers).

If you're currently building with `CC=clang`, switch to GCC for both:

```bash
CC=gcc CXX=g++ cargo build -p example-rtc-livekit-join
```

If `g++` reports `unrecognized command-line option '-Wno-changes-meaning'`, your
GCC is too old for the flags expected by the bundled WebRTC build. Use a newer
GCC (e.g., `g++-13` or later) and point both `CC` and `CXX` at that version:

```bash
CC=gcc-13 CXX=g++-13 cargo build -p example-rtc-livekit-join
```

If you see `TLS support not compiled in` when connecting to `wss://...`, ensure
the LiveKit crate is built with TLS enabled. You can force a TLS backend with
either of these:

```bash
cargo run -p example-rtc-livekit-join --features matrix-sdk-rtc-livekit/native-tls
# or
cargo run -p example-rtc-livekit-join --features matrix-sdk-rtc-livekit/rustls-tls
```

Only enable one TLS backend at a time; `native-tls` and `rustls-tls` are
mutually exclusive.

### Room identifier

`ROOM_ID` can be either a room id (`!roomid:example.org`) or a room alias
(`#alias:example.org`). If you provide an alias, set `VIA_SERVERS` to a
comma-separated list of server names to help the homeserver resolve the alias.

### LiveKit service URL

The example relies on the LiveKit service URL advertised by the homeserver's
`.well-known` discovery. If you see `Signal ... 404 Not Found` errors while
connecting to `wss://<homeserver>/rtc`, the homeserver is likely missing a
LiveKit focus or advertising the wrong URL. Ensure the server advertises a
correct LiveKit `service_url`, or use a homeserver that provides a LiveKit
endpoint that supports the `/rtc` signal path.

If your deployment exposes a separate endpoint (for example, Element Call's
`/sfu/get`), you can set `LIVEKIT_SFU_GET_URL`. The example will request an
OpenID token for the logged-in Matrix user, then POST `{ room, openid_token,
device_id }` to `/sfu/get` to obtain both the `service_url` and a LiveKit access
token. When `LIVEKIT_SFU_GET_URL` is set, the example uses the response values
instead of `LIVEKIT_SERVICE_URL` and `LIVEKIT_TOKEN`.

The LiveKit access token is appended to the `service_url` as the
`access_token` query parameter (matching Element Call's WebSocket usage) when
the URL does not already include one.

## What this example does

1. Logs into Matrix.
2. Joins the target room.
3. Starts the sync loop.
4. Runs `LiveKitRoomDriver`, which connects to LiveKit when the room has active
   call memberships and disconnects when they disappear.

## Call graph (example flow)

```mermaid
flowchart TD
    A[main()] --> B[Client::builder().build()]
    B --> C[login_username()]
    C --> D[get_room()/join_room_by_id_or_alias()]
    D --> E[tokio::spawn sync()]
    D --> F[resolve service url/token (env, /sfu/get, or .well-known)]
    F --> G[Room::subscribe_info()]
    F --> H[livekit_service_url()]
    H --> I[Client::rtc_foci()]
    F --> J[update_connection()]
    J --> K{has_active_room_call?}
    K -->|yes| L[LiveKitConnector::connect()]
    L --> M[LiveKitSdkConnector::connect()]
    M --> N[LiveKitTokenProvider::token()]
    M --> O[RoomOptions::default()]
    M --> P[Room::connect (LiveKit SDK)]
    K -->|no| Q[LiveKitConnection::disconnect()]
```
