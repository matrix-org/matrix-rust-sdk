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
LIVEKIT_TOKEN=your-token \
RUST_LOG=info \
cargo run -p example-rtc-livekit-join
```

## What this example does

1. Logs into Matrix.
2. Joins the target room.
3. Starts the sync loop.
4. Runs `LiveKitRoomDriver`, which connects to LiveKit when the room has active
   call memberships and disconnects when they disappear.

