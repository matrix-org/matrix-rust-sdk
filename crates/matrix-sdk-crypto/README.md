A no-network-IO implementation of a state machine that handles E2EE for
[Matrix] clients.

# Usage

If you're just trying to write a Matrix client or bot in Rust, you're probably
looking for [matrix-sdk] instead.

However, if you're looking to add E2EE to an existing Matrix client or library,
read on.

The state machine works in a push/pull manner:

- you push state changes and events retrieved from a Matrix homeserver /sync
  response into the state machine
- you pull requests that you'll need to send back to the homeserver out of the
  state machine

```rust,no_run
use std::collections::BTreeMap;

use matrix_sdk_crypto::{EncryptionSyncChanges, OlmMachine, OlmError};
use ruma::{
    api::client::sync::sync_events::{v3::ToDevice, DeviceLists},
    device_id, user_id,
};

#[tokio::main]
async fn main() -> Result<(), OlmError> {
    let alice = user_id!("@alice:example.org");
    let machine = OlmMachine::new(&alice, device_id!("DEVICEID")).await;

    let changed_devices = DeviceLists::default();
    let one_time_key_counts = BTreeMap::default();
    let unused_fallback_keys = Some(Vec::new());
    let next_batch_token = "T0K3N".to_owned();

    // Push changes that the server sent to us in a sync response.
    let decrypted_to_device = machine.receive_sync_changes(EncryptionSyncChanges {
        to_device_events: vec![],
        changed_devices: &changed_devices,
        one_time_keys_counts: &one_time_key_counts,
        unused_fallback_keys: unused_fallback_keys.as_deref(),
        next_batch_token: Some(next_batch_token),
    }).await?;

    // Pull requests that we need to send out.
    let outgoing_requests = machine.outgoing_requests().await?;

    // Send the requests here out and call machine.mark_request_as_sent().

    Ok(())
}
```

[Matrix]: https://matrix.org/
[matrix-sdk]: https://github.com/matrix-org/matrix-rust-sdk/

# Room key sharing algorithm

The decision tree below visualizes the way this crate decides whether a room
key will be shared with a requester upon a key request.

![](https://raw.githubusercontent.com/matrix-org/matrix-rust-sdk/main/contrib/key-sharing-algorithm/model.png)


# Crate Feature Flags

The following crate feature flags are available:

* `qrcode`: Enbles QRcode generation and reading code

* `testing`: provides facilities and functions for tests, in particular for integration testing store implementations. ATTENTION: do not ever use outside of tests, we do not provide any stability warantees on these, these are merely helpers. If you find you _need_ any function provided here outside of tests, please open a Github Issue and inform us about your use case for us to consider.
