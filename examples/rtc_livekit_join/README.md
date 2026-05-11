# LiveKit call join (skeleton)

This example shows how to use `matrix-sdk-rtc-livekit` to
join/leave a LiveKit room with Element-Call based SFrame Encryption based on MatrixRTC call memberships.

**Important:** this example needs MatrixRTC memberships (e.g. `m.call.member`)
for your device before the LiveKit room driver will connect.

## What this example does

1. Logs into Matrix. (HOMESERVER_URL, MATRIX_USERNAME and MATRIX_PASSWORD)
2. Backup from local CryptoStore if exists (MATRIX_RECOVERY_KEY)
3. Joins the target room (ROOM_ID)
4. Start the sync loop to collect Participant Keys
5. Send Membership Event and emit own Participant Key via Widget API
6. Runs the LiveKit room driver, which connects to LiveKit when the room has active
   call memberships and disconnects when they disappear.
7. With active connection to LiveKit Room the local Camera will be feeded into Element Call via V4L2

## What is working and what not?

- [x] Usage of Linux based Video Sources with V4L2 (also working with Embedded Linux Distros)
- [ ] Usage of Video Sources from other OSes
- [ ] Usage of Microphones
- [ ] Increase of Ratchet Index on Group Membership Change

## Usage

Configuration of the example happens with this environment variables:

### Mandatory Variables

```bash
HOMESERVER_URL=https://matrix.example.org \
MATRIX_USERNAME=@alice:example.org \
MATRIX_PASSWORD=secret \
ROOM_ID=!roomid:example.org \
ELEMENT_CALL_URL=https://webclient.matrix.example.org
```

### Optional Variables for Connection

```bash
MATRIX_DEVICE_ID="iABCdef"
MATRIX_RECOVERY_KEY="recov ery key client exam ple"
LIVEKIT_TOKEN=your-token \
LIVEKIT_SFU_GET_URL=https://demo.call.bundesmessenger.info/sfu/get \
LIVEKIT_SERVICE_URL=wss://livekit.example.org \
```

When defining static `MATRIX_DEVICE_ID` it is necessary to use the local crypto store (needs to compile sqlite feature). Otherwise the Device ID needs to be changed for every run because the same Device ID won't work twice. (It's a feature, not a bug)

### Optional Variables for Video Feeding

```bash
V4L2_DEVICE=/dev/video0 \
V4L2_VIDEO_SOURCE=camera \
V4L2_WIDTH=1280 \
V4L2_HEIGHT=720 \
```

To publish generated solid red test frames instead of a real camera stream, set
`V4L2_VIDEO_SOURCE=test_red` (or `test-red` / `red`). In this mode, `V4L2_DEVICE`
is optional and defaults to 640x480 when width/height are omitted.

### Optional Variables for Participant Key Provision

We have option for resend interval for own participant key. For now the Ratchet isn't increasing for Group Membership changes.

```bash
PER_PARTICIPANT_KEY_RESEND_SECS=30
```


## Build notes (Linux)

The LiveKit/WebRTC dependency currently links against libstdc++. If you build
with `clang`/`libc++`, you may see undefined references like
`std::__throw_bad_array_new_length()` or `std::__glibcxx_assert_fail`.

I had best compability over multiple different platforms by using gcc-13 with g++-13:

```
CC=gcc-13 CXX=g++-13 cargo build -p example-rtc-livekit-join --features rustls-tls --features v4l2,matrix-sdk/experimental-widgets,experimental-widgets,sqlite
```
