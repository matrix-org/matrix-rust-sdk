# LiveKit call join (skeleton)

This example shows how to wire `matrix-sdk-rtc` and `matrix-sdk-rtc-livekit` to
join/leave a LiveKit room based on MatrixRTC call memberships.

**Important:** this example does **not** publish MatrixRTC memberships (e.g.
`m.call.member`) for your device. You must integrate a membership publisher
(or Element Call) so the room contains an active call membership before the
`LiveKitRoomDriver` will connect.

## Usage

```bash
HOMESERVER_URL=https://matrix.example.org \
MATRIX_USERNAME=@alice:example.org \
MATRIX_PASSWORD=secret \
ROOM_ID=!roomid:example.org \
VIA_SERVERS=example.org,otherserver.org \
LIVEKIT_TOKEN=your-token \
RUST_LOG=info \
cargo run -p example-rtc-livekit-join
```

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
    D --> F[LiveKitSdkConnector::new()]
    F --> G[LiveKitRoomDriver::new()]
    G --> H[LiveKitRoomDriver::run()]
    H --> I[Room::subscribe_info()]
    H --> J[livekit_service_url()]
    J --> K[Client::rtc_foci()]
    H --> L[update_connection()]
    L --> M{has_active_room_call?}
    M -->|yes| N[LiveKitConnector::connect()]
    N --> O[LiveKitSdkConnector::connect()]
    O --> P[LiveKitTokenProvider::token()]
    O --> Q[RoomOptions::default()]
    O --> R[Room::connect (LiveKit SDK)]
    M -->|no| S[LiveKitConnection::disconnect()]
```
