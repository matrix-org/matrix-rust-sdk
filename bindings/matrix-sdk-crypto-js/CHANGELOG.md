# v0.1.0-alpha.6

## New APIs

-   Add new accessor `InboundGroupSession.senderKey`.
-   Add a new API, `OlmMachine.registerRoomKeyUpdatedCallback`, which
    applications can use to listen for received room keys.

## Bug fixes

-   In `OlmMachine.getUserDevices`, wait a limited time for any in-flight
    device-list updates to complete.
