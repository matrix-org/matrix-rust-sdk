# v0.1.0-alpha.12

## Changes in the Javascript bindings

-   In `OlmMachine.getIdentity`, wait a limited time for any in-flight
    device-list updates to complete.

# v0.1.0-alpha.11

## Changes in the Javascript bindings

-   Simplify the response type of `Sas.confirm()`.
-   Add `VerificationRequest.registerChangesCallback()`,
    `Sas.registerChangesCallback()`, and `Qr.registerChangesCallback()`.
-   Add `VerificationRequest.phase()` and `VerificationRequest.getVerification()`.

## Changes in the underlying Rust crate

-   Add support for the `hkdf-hmac-sha256.v2` SAS message authentication code.

-   Ensure that the correct short authentication strings are used when accepting a
    SAS verification with the `Sas::accept()` method.

# v0.1.0-alpha.10

-   Add `masterKey`, `userSigningKey`, `selfSigningKey` to `UserIdentity` and `OwnUserIdentity`

# v0.1.0-alpha.9

-   Extend `OlmDevice.markRequestAsSent` to accept responses to
    `SigningKeysUploadRequest`s.
-   Add a missing `const` for compatibility with ECMAScript Module compatibility
    mode.
-   Fix the body of `SignatureUploadRequest`s to match the spec.
-   Add a constructor for `SigningKeysUploadRequest`.

# v0.1.0-alpha.8

-   `importCrossSigningKeys`: change the parameters to be individual keys
    rather than a `CrossSigningKeyExport` object.
-   Make `unused_fallback_keys` optional in `Machine.receive_sync_changes`

# v0.1.0-alpha.7

-   Add new accessors `Device.algorithms` and `Device.isSignedByOwner`
-   In `OlmMachine.getUserDevices`, wait a limited time for any in-flight
    device-list updates to complete.

# v0.1.0-alpha.6

-   Add new accessor `InboundGroupSession.senderKey`.
-   Add a new API, `OlmMachine.registerRoomKeyUpdatedCallback`, which
    applications can use to listen for received room keys.
