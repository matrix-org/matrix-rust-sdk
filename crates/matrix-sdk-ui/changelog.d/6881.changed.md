[**breaking**] `Timeline::send_reply` now returns the `SendHandle` of the
queued reply, instead of `()`. Replies go through the send queue like any other
message, so callers can now track, abort or retry them, as they already can with
`Timeline::send`.
