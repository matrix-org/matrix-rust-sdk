The timeline's `AttachmentConfig` gained an `extra_content` field, and the
timeline a `send_with_extra_content()` method, forwarded to the underlying
send queue. Extra fields never override the fields of the event itself.
