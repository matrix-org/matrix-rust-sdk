Added `EventCacheStore::find_events_before_timestamp` to query events older 
than a given timestamp. Each result includes the event and its position in 
the linked chunk, so callers can remove expired events without scanning 
chunks manually.
