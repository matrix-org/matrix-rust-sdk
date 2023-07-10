# v0.7.0

- The `OlmMachine::export_cross_signing_keys()` method now returns a `Result`.
  This removes an `unwrap()` from the codebase.

- Add support for the `hkdf-hmac-sha256.v2` SAS message authentication code.

- Ensure that the correct short authentication strings are used when accepting a
  SAS verification with the `Sas::accept()` method.

- Add a new optional `message-ids` feature which adds a unique ID to the content
  of `m.room.encrypted` event contents which get sent out.

- Disable the automatic-key-forwarding feature by default.

- Add a new variant to the `VerificationRequestState` enum called
  `Transitioned`. This enum variant is used when a `VerificationRequest`
  transitions into a concrete `Verification` object. The concrete `Verification`
  object is given as associated data in the `Transitioned` enum variant.

- Replace the libolm backup encryption code with a native Rust version. This
  adds WASM support to the backups_v1 feature.

- Add new API `store::Store::room_keys_received_stream` to provide
  updates of room keys being received.

- Add new method `identities::device::Device::first_time_seen_ts`
  that allows to get a local timestamp of when the device was first seen by
  the sdk (in seconds since epoch).

- When rejecting a key-verification request over to-device messages, send the
  `m.key.verification.cancel` to the device that made the request, rather than
  broadcasting to all devices.

- Expose `VerificationRequest::time_remaining`.

- For verification-via-emojis, return the word "Aeroplane" rather than
  "Airplane", for consistency with the Matrix spec.

- Fix handling of SAS verification start events once we have shown a QR code.
