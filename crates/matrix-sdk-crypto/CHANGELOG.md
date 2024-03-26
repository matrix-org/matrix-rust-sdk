# UNRELEASED

Changed:

- Fallback keys are rotated in a time-based manner, instead of waiting for the
  server to tell us that a fallback key got used.
  ([#3151](https://github.com/matrix-org/matrix-rust-sdk/pull/3151))

Breaking changes:

- Rename the `OlmMachine::invalidate_group_session` method to
  `OlmMachine::discard_room_key`

- Move `OlmMachine::export_room_keys` to `matrix_sdk_crypto::store::Store`.
  (Call it with `olm_machine.store().export_room_keys(...)`.)

- Add new `dehydrated` property to `olm::account::PickledAccount`.
  ([#3164](https://github.com/matrix-org/matrix-rust-sdk/pull/3164))

Additions:

- Expose new method `OlmMachine::device_creation_time`.
  ([#3275](https://github.com/matrix-org/matrix-rust-sdk/pull/3275))

- Log more details about the Olm session after encryption and decryption.
  ([#3242](https://github.com/matrix-org/matrix-rust-sdk/pull/3242))

- When Olm message decryption fails, report the error code(s) from the failure.
  ([#3212](https://github.com/matrix-org/matrix-rust-sdk/pull/3212))

- Expose new methods `OlmMachine::set_room_settings` and
  `OlmMachine::get_room_settings`.
  ([#3042](https://github.com/matrix-org/matrix-rust-sdk/pull/3042))

- Add new properties `session_rotation_period` and
  `session_rotation_period_msgs` to `store::RoomSettings`.
  ([#3042](https://github.com/matrix-org/matrix-rust-sdk/pull/3042))

- Fix bug which caused `SecretStorageKey` to incorrectly reject secret storage
  keys whose metadata lacked check fields.
  ([#3046](https://github.com/matrix-org/matrix-rust-sdk/pull/3046))

- Add new API `Device::encrypt_event_raw` that allows
  to encrypt an event to a specific device.
  ([#3091](https://github.com/matrix-org/matrix-rust-sdk/pull/3091))

- Add new API `store::Store::export_room_keys_stream` that provides room
  keys on demand.

- Include event timestamps on logs from event decryption.
  ([#3194](https://github.com/matrix-org/matrix-rust-sdk/pull/3194))

# 0.7.0

- Add method to mark a list of inbound group sessions as backed up:
  `CryptoStore::mark_inbound_group_sessions_as_backed_up`

- `OlmMachine::toggle_room_key_forwarding` is replaced by two separate methods:

  * `OlmMachine::set_room_key_requests_enabled`, which controls whether
    outgoing room key requests are enabled, and:

  * `OlmMachine::set_room_key_forwarding_enabled`, which controls whether we
    automatically reply to incoming room key requests.

  `OlmMachine::is_room_key_forwarding_enabled` is updated to return the setting
  of `OlmMachine::set_room_key_forwarding_enabled`, while
  `OlmMachine::are_room_key_requests_enabled` is added to return the setting of
  `OlmMachine::set_room_key_requests_enabled`.

  ([#2902](https://github.com/matrix-org/matrix-rust-sdk/pull/2902))

- Improve performance of `share_room_key`.
  ([#2862](https://github.com/matrix-org/matrix-rust-sdk/pull/2862))

- `get_missing_sessions`: Don't block waiting for `/keys/query` requests on
  blacklisted servers, and improve performance.
  ([#2845](https://github.com/matrix-org/matrix-rust-sdk/pull/2845))

- Generalize `olm::Session::encrypt` to accept any value implementing
  `Serialize` for the `value` parameter, instead of specifically
  `serde_json::Value`. Note that references to `Serialize`-implementing types
  themselves implement `Serialize`.

- Change the argument to `OlmMachine::receive_sync_changes` to be an
  `EncryptionSyncChanges` struct packing all the arguments instead of many
  single arguments. The new `next_batch_token` field there should be the
  `next_batch` value read from the latest sync response.

- Handle missing devices in `/keys/claim` responses.
  ([#2805](https://github.com/matrix-org/matrix-rust-sdk/pull/2805))

- Add the higher level decryption method `decrypt_session_data` to the
  `BackupDecryptionKey` type.

- Add a higher level method to create signatures for the backup info. The
  `OlmMachine::backup_machine()::sign_backup()` method can be used to add
  signatures to a `RoomKeyBackupInfo`.

- Remove the `backups_v1` feature, backups support is now enabled by default.

- Use the `Signatures` type as the return value for the
  `MegolmV1BackupKey::signatures()` method.

- Add two new methods to import room keys,
  `OlmMachine::store()::import_exported_room_keys()` for file exports and
  `OlmMachine::backup_machine()::import_backed_up_room_keys()` for backups. The
  `OlmMachine::import_room_keys()` method is now deprecated.

- The parameter order of `OlmMachine::encrypt_room_event_raw` and
  `OutboundGroupSession::encrypt` has changed, `content` is now last
  - The parameter type of `content` has also changed, from `serde_json::Value`
    to `&Raw<AnyMessageLikeEventContent>`

- Change the return value of `bootstrap_cross_signing` so it returns an extra
  keys upload request.  The three requests must be sent in the order they
  appear in the return tuple.

- Stop logging large quantities of data about the `Store` during olm
  decryption.

- Remove spurious "Unknown outgoing secret request" warning which was logged
  for every outgoing secret request.

- Clean up the logging of to-device messages in `share_room_key`.

- Expose new `OlmMachine::get_room_event_encryption_info` method.

- Add support for secret storage.

- Add initial support for MSC3814 - dehydrated devices.

- Mark our `OwnUserIdentity` as verified if we successfully import the matching
  private keys.

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

- Fix a bug which could cause generated one-time-keys not to be persisted.

- Fix parsing error for `POST /_matrix/client/v3/keys/signatures/upload`
  responses generated by Synapse.

- Add new API `OlmMachine::query_keys_for_users` for generating out-of-band key
  queries.

- Rename "recovery key" to "backup decryption key" to avoid confusion with the
  secret-storage key which is also known as a recovery key.

  This affects the `matrix_sdk_crypto::store::RecoveryKey` struct itself (now
  renamed to `BackupDecryptionKey`, as well as
  `BackupMachine::save_recovery_key` (now `save_decryption_key`).

- Change the returned success value type of `BackupMachine::backup` from
  `OutgoingRequest` to `(OwnedTransactionId, KeysBackupRequest)`.
