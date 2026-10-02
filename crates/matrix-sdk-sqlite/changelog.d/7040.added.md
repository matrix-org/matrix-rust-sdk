Added migration 019 to store each event's `origin_server_ts` in its own indexed
column. This lets the store find expired events by timestamp without decrypting
every event blob. Added `find_events_before_timestamp` to use that index.
