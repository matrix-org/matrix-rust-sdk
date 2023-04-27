# v0.1.0-alpha.8

-  Make `unused_fallback_keys` as optional in `Machine.receive_sync_changes`

# v0.1.0-alpha.7

-   Add new accessors `Device.algorithms` and `Device.isSignedByOwner`
-   In `OlmMachine.getUserDevices`, wait a limited time for any in-flight
    device-list updates to complete.

# v0.1.0-alpha.6

-   Add new accessor `InboundGroupSession.senderKey`.
-   Add a new API, `OlmMachine.registerRoomKeyUpdatedCallback`, which
    applications can use to listen for received room keys.
