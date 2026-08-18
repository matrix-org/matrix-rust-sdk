Fix a possible overflow panic in Tantivy when a malformed timestamp is
converted from milliseconds to nanoseconds. If the timestamp is too large,
the conversion to nanoseconds was panicking. The timestamp now comes from
`TimelineEvent::timestamp`, which already deals with malformed timestamp. In
addition, the timestamp is capped in case the API is misused.
