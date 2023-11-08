# FFI bindings for the rust matrix SDK

This uses [`uniffi`](https://mozilla.github.io/uniffi-rs/Overview.html) to build the matrix bindings for native support and wasm-bindgen for web-browser assembly support. Please refer to the specific section to figure out how to build and use the bindings for your platform.

# OpenTelemetry support

The bindings have support for OpenTelemetry, allowing for the upload of traces to
any OpenTelemetry collector. This support is provided through the
[`opentelemetry`](https://docs.rs/opentelemetry/latest/opentelemetry/)
crate, which requires the [`protoc`] binary to be installed during the build
process.

## Platforms

### Swift/iOS sync

### Swift/iOS async

TBD

### Example

This is a rough overview example how to use the FFI bindings. You need to implement the callbacks and combine that with the intial results to build your room list and timelines.

```
class Matrix {
  final MyRoomListEntriesListener myRoomListEntriesListener = MyRoomListEntriesListener();

  Client matrixClientLogin() {
    var client = ClientBuilder().username("<accountname>").build();
    client.login("<accountname>", "<accountpassword>", null, null);

    // Create a Sync Service
    var syncService = await client.sync_service().finish();
    // Start a Sync Service
    syncService.start();
    // Create a Room Service
    var roomService = await ss.room_list_service().all_rooms();
    // Attach your callback to get notitified when rooms are availible/found
    roomService.entries(myRoomListEntriesListener)

    return client;
  }
}

void main() async {
  // Create a timeline listener. Have a look here for an implementation
  // https://github.com/vector-im/element-x-android/blob/develop/libraries/matrix/impl/src/main/kotlin/io/element/android/libraries/matrix/impl/timeline/MatrixTimelineDiffProcessor.kt
  final MyTimelineListener myTimelineListener = MyTimelineListener();

  Matrix matrix = Matrix();
  Client client = await m.matrixClientLogin();

  // Here you should wait for your callback to trigger.

  // Get the rooms from the client
  List<Room> rooms = c.rooms();

  rooms.forEach((room) => {
    var result = await room.add_timeline_listener(myTimelineListener);

    // To go backwards to get more events
    room.paginate_backwards(opts);

    // Send a message into the room.
    room.send(message_event_content_from_markdown("Hello there"));
  });
}
```
