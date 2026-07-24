[**breaking**] The `OlmMachine::receive_sync_changes()` method now correctly
treats a missing one-time key count to mean that zero one-time keys exist on the
homeserver.

A new method `OlmMachine::receive_sync_changes_msc4186()` was added if the
MSC4816 semantics for one-time key counts should be used.
