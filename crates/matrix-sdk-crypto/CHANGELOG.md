# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### Features

- Added a new field `forwarder` to `InboundGroupSession` of type `ForwarderData`, which stores information about the forwarder of a session shared in a room key bundle under [MSC4268](https://github.com/matrix-org/matrix-spec-proposals/pull/4268).
  ([#5980])(https://github.com/matrix-org/matrix-rust-sdk/pull/5980)
- The `OutboundGroupSession` and `OlmMachine` now return the `EncryptionInfo` 
  used when encrypting raw events.
  ([#5936](https://github.com/matrix-org/matrix-rust-sdk/pull/5936))
- Expose a new method `CryptoStore::has_downloaded_all_room_keys`, used to track whether the
  client has previously downloaded historical room keys for a given room from key backup prior
  to building an [MSC4268](https://github.com/matrix-org/matrix-spec-proposals/pull/4268) room
  key bundle.
  ([#6017](https://github.com/matrix-org/matrix-rust-sdk/pull/6017))
  ([#6044](https://github.com/matrix-org/matrix-rust-sdk/pull/6044))


### Refactor

- [**breaking**] The `message-ids` feature has been removed. It was already a no-op and has now
  been eliminated entirely.
  ([#5963](https://github.com/matrix-org/matrix-rust-sdk/pull/5963))

## [0.16.0] - 2025-12-04

### Features

- When we receive an inbound Megolm session from two different sources, merge the two copies together to get the best of both.
  ([#5865](https://github.com/matrix-org/matrix-rust-sdk/pull/5865)
- When constructing a key bundle for history sharing, if we had received a key bundle ourselves, in which one or more sessions was marked as "history not shared", pass that on to the new user.
  ([#5820](https://github.com/matrix-org/matrix-rust-sdk/pull/5820)
- Expose new method `CryptoStore::get_withheld_sessions_by_room_id`.
  ([#5819](https://github.com/matrix-org/matrix-rust-sdk/pull/5819))
- Use new withheld code in key bundles for sessions not marked as
  `shared_history`.
  ([#5807](https://github.com/matrix-org/matrix-rust-sdk/pull/5807), ([#5834](https://github.com/matrix-org/matrix-rust-sdk/pull/5834))
- Improve feedback support for shared history when downloading room key
  bundles.
  ([#5737](https://github.com/matrix-org/matrix-rust-sdk/pull/5737))
  - Add `RoomKeyWithheldEntry` enum, wrapping either a received to-device `m.room_key.withheld` event or
    its content, if derived from a downloaded room key bundle.
  - `OlmMachine::receive_room_key_bundle` now appends withheld key information to the store.
  - [**breaking**] `Changes::withheld_session_info` now stores a `RoomKeyWithheldEntry` in each `room-id`-`session-id` entry.
  - [**breaking**] `CryptoStore::get_withheld_info` now returns `Result<Option<RoomKeyWithheldEntry>>`. This change also affects `MemoryStore`.
- [**breaking**] Add `name` fields to some of the variants of
  `store::SecretImportError` to indicate what secret was being imported when the
  error occurred.
  ([#5647](https://github.com/matrix-org/matrix-rust-sdk/pull/5647))

### Bug Fixes

- Fix a bug which caused encrypted to-device messages from unknown devices to be ignored.
  ([#5763](https://github.com/matrix-org/matrix-rust-sdk/pull/5763))
- Fix a bug which caused history shared on invite to be ignored when "exclude insecure devices" was enabled.
  ([#5763](https://github.com/matrix-org/matrix-rust-sdk/pull/5763))
- Fix a bug introduced in 0.14.0 which meant that the serialization of the value returned by `OtherUserIdentity::verification_request_content` did not include a `msgtype` field.
  ([#5642](https://github.com/matrix-org/matrix-rust-sdk/pull/5642))

## [0.14.0] - 2025-09-04

### Features

- Log message index for Megolm sessions received over encrypted to-device messages. ([#5599](https://github.com/matrix-org/matrix-rust-sdk/pull/5599))
- Add `RoomSettings::encrypt_state_events` flag. ([#5511](https://github.com/matrix-org/matrix-rust-sdk/pull/5511))
- Make sure to accept historic room key bundles only if the sender is trusted
  enough.
  ([#5510](https://github.com/matrix-org/matrix-rust-sdk/pull/5510))
- [**breaking**]: When in "exclude insecure devices" mode, refuse to decrypt
  incoming to-device messages from unverified devices, except for some
  exceptions for certain event types. To support this, a new variant has been
  added to `ProcessedToDeviceEvent`: `UnverifiedSender`, which is returned from
  `OlmMachine::receive_sync_changes` when we are excluding insecure devices and
  the sender's device is not verified. Also, several methods now take a
  `DecryptionSettings` argument to allow controlling the processing of to-device
  events based on those settings. To recreate the previous behaviour pass in:
  `DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted }`.
  Affected methods are `OlmMachine::receive_sync_changes`,
  `RehydratedDevice::receive_events`, and several internal methods.
  ([#5319](https://github.com/matrix-org/matrix-rust-sdk/pull/5319))
- [**breaking**] The `Device::encrypt_event_raw` and (experimental)
  `OlmMachine::encrypt_content_for_devices` have new `share_strategy` parameters
  to ensure that the recipients are sufficiently trusted.
  ([#5457](https://github.com/matrix-org/matrix-rust-sdk/pull/5457/))

### Refactor

- [**breaking**] The `sender_key` and `device_id` fields of
  `encrypted::MegolmV1AesSha2Content` and
  `room_key_request::MegolmV1AesSha2Content` are now optional. The have been
  deprecated in Matrix 1.3 and are no longer required.
  ([#5489](https://github.com/matrix-org/matrix-rust-sdk/pull/5489))

## [0.13.0] - 2025-07-10

### Features

- [**breaking**] Add a new `VerificationLevel::MismatchedSender` to indicate that the sender of an event appears to have been tampered with.
  ([#5219](https://github.com/matrix-org/matrix-rust-sdk/pull/5219))

### Refactor

- [**breaking**] The `PendingChanges`, `Changes`, `StoredRoomKeyBundleData`,
  `TrackedUser`, `IdentityChanges`, `DeviceChanges`, `DeviceUpdates`,
  `IdentityUpdates`, `BackupDecryptionKey`, `DehydratedDeviceKey`,
  `RoomKeyCounts`, `BackupKeys`, `CrossSigningKeyExport`, `UserKeyQueryResult`,
  `RoomSettings`, `RoomKeyInfo`, and `RoomKeyWithheldInfo` types have been moved
  from the `store` module into a new `store/types` module.
  ([#5177](https://github.com/matrix-org/matrix-rust-sdk/pull/5177))

## [0.12.0] - 2025-06-10

### Features

- [**breaking**] The `ProcessedToDeviceEvent::Decrypted` variant now also have an `EncryptionInfo` field.
  Format changed from `Decrypted(Raw<AnyToDeviceEvent>)` to `Decrypted { raw: Raw<AnyToDeviceEvent>, encryption_info: EncryptionInfo) }`
  ([#5074](https://github.com/matrix-org/matrix-rust-sdk/pull/5074))

- [**breaking**] Move `session_id` from `EncryptionInfo` to `AlgorithmInfo` as it is megolm specific.
  Use `EncryptionInfo::session_id()` helper for quick access.
  ([#4981](https://github.com/matrix-org/matrix-rust-sdk/pull/4981))

- Send stable identifier `sender_device_keys` for MSC4147 (Including device
  keys with Olm-encrypted events).
  ([#4964](https://github.com/matrix-org/matrix-rust-sdk/pull/4964))

- Add experimental APIs for sharing encrypted room key history with new members, `Store::build_room_key_bundle` and `OlmMachine::share_room_key_bundle_data`.
  ([#4775](https://github.com/matrix-org/matrix-rust-sdk/pull/4775), [#4864](https://github.com/matrix-org/matrix-rust-sdk/pull/4864))

- Check the `sender_device_keys` field on *all* incoming Olm-encrypted to-device messages
  and ignore any to-device messages which include the field but whose data is invalid
  (as per [MSC4147](https://github.com/matrix-org/matrix-spec-proposals/pull/4147)).
  ([#4922](https://github.com/matrix-org/matrix-rust-sdk/pull/4922))

- Fix bug which caused room keys to be unnecessarily rotated on every send in the
  presence of blacklisted/withheld devices in the room.
  ([#4954](https://github.com/matrix-org/matrix-rust-sdk/pull/4954))

- Fix [#2729](https://github.com/matrix-org/matrix-rust-sdk/issues/2729) which in rare
  cases can cause room key oversharing.
  ([#4975](https://github.com/matrix-org/matrix-rust-sdk/pull/4975))

- [**breaking**] `OlmMachine.receive_sync_changes` returns now a list of `ProcessedToDeviceEvent`
  instead of a list of `Raw<AnyToDeviceEvent>`. With variants like `Decrypted`|`UnableToDecrypt`|`PlainText`|`NotProcessed`.
  This allows for example to make the difference between an event sent in clear and an event successfully decrypted.
  For quick compatibility a helper `ProcessedToDeviceEvent::to_raw` allows to map back to the previous behaviour.
  ([#4935](https://github.com/matrix-org/matrix-rust-sdk/pull/4935))

## [0.11.1] - 2025-06-10

### Security Fixes
- Check the sender of an event matches owner of session, preventing sender
  spoofing by homeserver owners.
  [13c1d20](https://github.com/matrix-org/matrix-rust-sdk/commit/13c1d2048286bbabf5e7bc6b015aafee98f04d55) (High, [CVE-2025-48937](https://www.cve.org/CVERecord?id=CVE-2025-48937), [GHSA-x958-rvg6-956w](https://github.com/matrix-org/matrix-rust-sdk/security/advisories/GHSA-x958-rvg6-956w)).

### Bug Fixes
- Remove a wildcard enum variant import which breaks compilation if used with
  `tracing-attributes` version `0.1.29`. This is a workaround for a bug in
  `tracing-attributes`.
  ([#5190](https://github.com/matrix-org/matrix-rust-sdk/issues/5190)) ([#5191](https://github.com/matrix-org/matrix-rust-sdk/issues/5191)) ([#5193](https://github.com/matrix-org/matrix-rust-sdk/issues/5193))

## [0.11.0] - 2025-04-11

### Features

- [**breaking**] Add support for the shared history flag defined in
  [MSC3061](https://github.com/matrix-org/matrix-spec-proposals/pull/3061).
  The shared history flag is now respected when room keys are received as an
  `m.room_key` event as well as when they are imported from a backup or a file
  export. We also ensure to set the flag when we send out room keys. Due to
  this, a new argument to the constructor for `room_key::MegolmV1AesSha2Content`
  has been added and `PickledInboundGroupSession` has received a new
  `shared_history` field that defaults to `false.`
  ([#4700](https://github.com/matrix-org/matrix-rust-sdk/pull/4700))

- Have the `RoomIdentityProvider` return processing changes when identities transition
  to `IdentityState::Verified` too.
  ([#4670](https://github.com/matrix-org/matrix-rust-sdk/pull/4670))

## [0.10.0] - 2025-02-04

### Features

- [**breaking**] `CollectStrategy::DeviceBasedStrategy` is now split into three
  separate strategies (`AllDevices`, `ErrorOnVerifiedUserProblem`,
  `OnlyTrustedDevices`), to make the behaviour clearer.
  ([#4581](https://github.com/matrix-org/matrix-rust-sdk/pull/4581))

- Accept stable identifier `sender_device_keys` for MSC4147 (Including device
  keys with Olm-encrypted events).
  ([#4420](https://github.com/matrix-org/matrix-rust-sdk/pull/4420))

- Room keys are not shared with unsigned dehydrated devices.
  ([#4551](https://github.com/matrix-org/matrix-rust-sdk/pull/4551))

## [0.9.0] - 2024-12-18

### Features

- [**breaking**] Expose new API
  `DehydratedDevices::get_dehydrated_device_pickle_key`,
  `DehydratedDevices::save_dehydrated_device_pickle_key` and
  `DehydratedDevices::delete_dehydrated_device_pickle_key` to store/load the
  dehydrated device pickle key. This allows client to automatically rotate
  the dehydrated device to avoid one-time-keys exhaustion and to_device
  accumulation.
  `DehydratedDevices::keys_for_upload` and
  `DehydratedDevices::rehydrate` now use the `DehydratedDeviceKey` as parameter
  instead of a raw byte array. Use `DehydratedDeviceKey::from_bytes` to migrate.
  ([#4383](https://github.com/matrix-org/matrix-rust-sdk/pull/4383))

- Add extra logging in `OtherUserIdentity::pin_current_master_key` and
  `OtherUserIdentity::withdraw_verification`.
  ([#4415](https://github.com/matrix-org/matrix-rust-sdk/pull/4415))

- Added new `UtdCause` variants `WithheldForUnverifiedOrInsecureDevice` and `WithheldBySender`.
  These variants provide clearer categorization for expected Unable-To-Decrypt (UTD) errors
  when the sender either did not wish to share or was unable to share the room_key.
  ([#4305](https://github.com/matrix-org/matrix-rust-sdk/pull/4305))

- `UtdCause` has two new variants that replace the existing `HistoricalMessage`:
  `HistoricalMessageAndBackupIsDisabled` and `HistoricalMessageAndDeviceIsUnverified`.
  These give more detail about what went wrong and allow us to suggest to users
  what actions they can take to fix the problem. See the doc comments on these
  variants for suggested wording.
  ([#4384](https://github.com/matrix-org/matrix-rust-sdk/pull/4384))

## [0.8.0] - 2024-11-19

### Features

- Pin identity when we withdraw verification.

- Expose new method `OlmMachine::room_keys_withheld_received_stream`, to allow
  applications to receive notifications about received `m.room_key.withheld`
  events.
  ([#3660](https://github.com/matrix-org/matrix-rust-sdk/pull/3660)),
  ([#3674](https://github.com/matrix-org/matrix-rust-sdk/pull/3674))

- Expose new method `OlmMachine::clear_crypto_cache()`, with FFI bindings.
  ([#3462](https://github.com/matrix-org/matrix-rust-sdk/pull/3462))

- Expose new method `OlmMachine::upload_device_keys()`.
  ([#3457](https://github.com/matrix-org/matrix-rust-sdk/pull/3457))

- Expose new method `CryptoStore::import_room_keys`.
  ([#3448](https://github.com/matrix-org/matrix-rust-sdk/pull/3448))

- Expose new method `BackupMachine::backup_version`.
  ([#3320](https://github.com/matrix-org/matrix-rust-sdk/pull/3320))

- Add data types to parse the QR code data for the QR code login defined in.
  [MSC4108](https://github.com/matrix-org/matrix-spec-proposals/pull/4108)

- Expose new method `CryptoStore::clear_caches`.
  ([#3338](https://github.com/matrix-org/matrix-rust-sdk/pull/3338))

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


### Refactor

- Fix [#4424](https://github.com/matrix-org/matrix-rust-sdk/issues/4424) Failed
  storage upgrade for "PreviouslyVerifiedButNoLonger". This bug caused errors to
  occur when loading crypto information from storage, which typically prevented
  apps from starting correctly.
  ([#4430](https://github.com/matrix-org/matrix-rust-sdk/pull/4430))

- Add new method `OlmMachine::try_decrypt_room_event`.
  ([#4116](https://github.com/matrix-org/matrix-rust-sdk/pull/4116))

- Add reason code to `matrix_sdk_common::deserialized_responses::UnableToDecryptInfo`.
  ([#4116](https://github.com/matrix-org/matrix-rust-sdk/pull/4116))

- [**breaking**] The `UserIdentity` struct has been renamed to `OtherUserIdentity`.
  ([#4036](https://github.com/matrix-org/matrix-rust-sdk/pull/4036]))

- [**breaking**] The `UserIdentities` enum has been renamed to `UserIdentity`.
  ([#4036](https://github.com/matrix-org/matrix-rust-sdk/pull/4036]))

- Change the withheld code for keys not shared due to the
  `IdentityBasedStrategy`, from `m.unauthorised` to `m.unverified`.
  ([#3985](https://github.com/matrix-org/matrix-rust-sdk/pull/3985))

- Improve logging for undecryptable Megolm events.
  ([#3989](https://github.com/matrix-org/matrix-rust-sdk/pull/3989))

- Miscellaneous improvements to logging for verification and `OwnUserIdentity`
  updates.
  ([#3949](https://github.com/matrix-org/matrix-rust-sdk/pull/3949))

- Update `SenderData` on existing inbound group sessions when we receive
  updates via `/keys/query`.
  ([#3849](https://github.com/matrix-org/matrix-rust-sdk/pull/3849))

- Add message IDs to all outgoing to-device messages encrypted by
  `matrix-sdk-crypto`. The `message-ids` feature of `matrix-sdk-crypto` and
  `matrix-sdk-base` is now a no-op.
  ([#3776](https://github.com/matrix-org/matrix-rust-sdk/pull/3776))

- Log the content of received `m.room_key.withheld` to-device events.
  ([#3591](https://github.com/matrix-org/matrix-rust-sdk/pull/3591))

- Attempt to decrypt bundled events (reactions and the latest thread reply) if
  they are found in the unsigned part of an event.
  ([#3468](https://github.com/matrix-org/matrix-rust-sdk/pull/3468))

- Sign the device keys with the user-identity (i.e. cross-signing keys) if
  we're uploading the device keys and if the cross-signing keys are available.
  This approach eliminates the need to upload signatures in a separate request,
  ensuring that other users/devices will never encounter this device without a
  signature from their user identity. Consequently, they will never see the
  device as unverified.
  ([#3453](https://github.com/matrix-org/matrix-rust-sdk/pull/3453))

- Avoid emitting entries from `identities_stream_raw` and `devices_stream` when
  we receive a `/keys/query` response which shows that no devices changed.
  ([#3442](https://github.com/matrix-org/matrix-rust-sdk/pull/3442))

- Fallback keys are rotated in a time-based manner, instead of waiting for the
  server to tell us that a fallback key got used.
  ([#3151](https://github.com/matrix-org/matrix-rust-sdk/pull/3151))

Breaking changes:

- [**breaking**] `VerificationRequestState::Transitioned` now includes a new field
  `other_device_data` of type `DeviceData`.
  ([#4153](https://github.com/matrix-org/matrix-rust-sdk/pull/4153))

- [**breaking**] `OlmMachine::decrypt_room_event` now returns a `DecryptedRoomEvent` type,
  instead of the more generic `TimelineEvent` type.

- [**breaking**] **NOTE**: this version causes changes to the format of the serialised data in
  the CryptoStore, meaning that, once upgraded, it will not be possible to roll
  back applications to earlier versions without breaking user sessions.

- [**breaking**] Renamed `VerificationLevel::PreviouslyVerified` to
  `VerificationLevel::VerificationViolation`.

- [**breaking**] `OlmMachine::decrypt_room_event` now takes a
  `DecryptionSettings` argument, which includes a `TrustRequirement` indicating
  the required trust level for the sending device.  When it is called with
  `TrustRequirement` other than `TrustRequirement::Unverified`, it may return
  the new `MegolmError::SenderIdentityNotTrusted` variant if the sending device
  does not satisfy the required trust level.
  ([#3899](https://github.com/matrix-org/matrix-rust-sdk/pull/3899))

- [**breaking**] Change the structure of the `SenderData` enum to separate
  variants for previously-verified, unverified and verified.
  ([#3877](https://github.com/matrix-org/matrix-rust-sdk/pull/3877))

- [**breaking**] Where `EncryptionInfo` is returned it may include the new
  `PreviouslyVerified` variant of `VerificationLevel` to indicate that the user
  was previously verified and is no longer verified.
  ([#3877](https://github.com/matrix-org/matrix-rust-sdk/pull/3877))

- [**breaking**] Expose new methods `OwnUserIdentity::was_previously_verified`,
  `OwnUserIdentity::withdraw_verification`, and
  `OwnUserIdentity::has_verification_violation`, which track whether our own
  identity was previously verified.
  ([#3846](https://github.com/matrix-org/matrix-rust-sdk/pull/3846))

- [**breaking**] Add a new `error_on_verified_user_problem` property to
  `CollectStrategy::DeviceBasedStrategy`, which, when set, causes
  `OlmMachine::share_room_key` to fail with an error if any verified users on
  the recipient list have unsigned devices, or are no longer verified.

  When `CallectStrategy::IdentityBasedStrategy` is used,
  `OlmMachine::share_room_key` will fail with an error if any verified users on
  the recipient list are no longer verified, or if our own device is not
  properly cross-signed.

  Also remove `CollectStrategy::new_device_based`: callers should construct a
  `CollectStrategy::DeviceBasedStrategy` directly.

  `EncryptionSettings::new` now takes a `CollectStrategy` argument, instead of a
  list of booleans.
  ([#3810](https://github.com/matrix-org/matrix-rust-sdk/pull/3810))
  ([#3816](https://github.com/matrix-org/matrix-rust-sdk/pull/3816))
  ([#3896](https://github.com/matrix-org/matrix-rust-sdk/pull/3896))

- [**breaking**] Remove the method `OlmMachine::clear_crypto_cache()`, crypto
  stores are not supposed to have any caches anymore.

- [**breaking**] Add a `custom_account` argument to the
  `OlmMachine::with_store()` method, this allows users to learn their identity
  keys before they get access to the user and device ID.
  ([#3451](https://github.com/matrix-org/matrix-rust-sdk/pull/3451))

- [**breaking**] Add a `backup_version` argument to `CryptoStore`'s
  `inbound_group_sessions_for_backup`,
  `mark_inbound_group_sessions_as_backed_up` and `inbound_group_session_counts`
  methods. ([#3253](https://github.com/matrix-org/matrix-rust-sdk/pull/3253))

- [**breaking**] Rename the `OlmMachine::invalidate_group_session` method to
  `OlmMachine::discard_room_key`.

- [**breaking**] Move `OlmMachine::export_room_keys` to `matrix_sdk_crypto::store::Store`.
  (Call it with `olm_machine.store().export_room_keys(...)`.)

- [**breaking**] Add new `dehydrated` property to `olm::account::PickledAccount`.
  ([#3164](https://github.com/matrix-org/matrix-rust-sdk/pull/3164))

- [**breaking**] Remove deprecated `OlmMachine::import_room_keys`.
  ([#3448](https://github.com/matrix-org/matrix-rust-sdk/pull/3448))

- [**breaking**] Add the `SasState::Created` variant to differentiate the state between the
  party that sent the verification start and the party that received it.

- [**breaking**] Deprecate `BackupMachine::import_backed_up_room_keys`.
  ([#3448](https://github.com/matrix-org/matrix-rust-sdk/pull/3448))


# 0.7.2

### Security Fixes

- Fix `UserIdentity::is_verified` to take into account our own identity
  [#d8d9dae](https://github.com/matrix-org/matrix-rust-sdk/commit/d8d9dae9d77bee48a2591b9aad9bd2fa466354cc) (Moderate, [GHSA-4qg4-cvh2-crgg](https://github.com/matrix-org/matrix-rust-sdk/security/advisories/GHSA-4qg4-cvh2-crgg)).

# 0.7.1
### Security Fixes

- Don't log the private part of the backup key, introduced in [#71136e4](https://github.com/matrix-org/matrix-rust-sdk/commit/71136e44c03c79f80d6d1a2446673bc4d53a2067).

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
