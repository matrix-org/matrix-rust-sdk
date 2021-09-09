A no-io implementation of a state machine that handles E2EE for [Matrix] clients.

# Usage

This is probably not the crate you are looking for, it’s used internally in the [matrix-sdk].

If you're still interested in this crate it can be used to introduce E2EE
support into your client or client library.

The state machine works in a push/pull manner, you push state changes and events
that we receive from a sync response from the server, and we pull requests that
we need to send to the server out of the state machine.

```rust,no_run
use std::{collections::BTreeMap, convert::TryFrom};

use matrix_sdk_crypto::{OlmMachine, OlmError};
use ruma::{UserId, api::client::r0::sync::sync_events::{ToDevice, DeviceLists}};

#[tokio::main]
async fn main() -> Result<(), OlmError> {
    let alice = UserId::try_from("@alice:example.org").unwrap();
    let machine = OlmMachine::new(&alice, "DEVICEID".into());

    let to_device_events = ToDevice::default();
    let changed_devices = DeviceLists::default();
    let one_time_key_counts = BTreeMap::default();

    // Push changes that the server sent to us in a sync response.
    let decrypted_to_device = machine.receive_sync_changes(
        to_device_events,
        &changed_devices,
        &one_time_key_counts
    ).await?;

    // Pull requests that we need to send out.
    let outgoing_requests = machine.outgoing_requests().await?;

    // Send the requests here out and call machine.mark_request_as_sent().

    Ok(())
}
```

[Matrix]: https://matrix.org/
[matrix-sdk]: https://github.com/matrix-org/matrix-rust-sdk/
