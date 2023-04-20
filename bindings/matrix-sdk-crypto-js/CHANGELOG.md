# v0.1.0-alpha.7

-   In `OlmMachine.getUserDevices`, wait a limited time for any in-flight
    device-list updates to complete.

# v0.1.0-alpha.6

-   Add new accessor `InboundGroupSession.senderKey`.
-   Add a new API, `OlmMachine.registerRoomKeyUpdatedCallback`, which
    applications can use to listen for received room keys.
