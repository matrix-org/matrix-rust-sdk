Added `Client::wait_for_sync()`, which returns a future that'll complete whenever a sync response
has been processed by the client. Works with both sync v2 and simplified slidying sync.
