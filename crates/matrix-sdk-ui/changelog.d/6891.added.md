Add `Timeline::send_location`, moved from the FFI bindings. It builds a
`m.location` room message, sends it through the send queue, and returns the
`SendHandle`. It optionally sends the location as a reply, with the same
semantics as `Timeline::send_reply`.
