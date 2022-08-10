## Synced State
This method waits for the event spawned by a successful API call to be synced with the SDK's state. It therefore requires the concurrent receival of events. One can sync events using any of these methods:
+ [Client::sync_once][sync_once]
+ [Client::sync][sync]
+ [Client::sync_stream][sync_stream]
+ [Client::sync_with_callback][sync_with_callback]

[sync_once]: crate::Client::sync_once
[sync]: crate::Client::sync
[sync_stream]: crate::Client::sync_stream
[sync_with_callback]: crate::Client::sync_with_callback
