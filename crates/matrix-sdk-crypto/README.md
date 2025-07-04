A no-network-IO implementation of a state machine that handles end-to-end
encryption for [Matrix] clients.

If you're just trying to write a Matrix client or bot in Rust, you're probably
looking for [matrix-sdk] instead.

However, if you're looking to add end-to-end encryption to an existing Matrix
client or library, read on.

The state machine works in a push/pull manner:

- you push state changes and events retrieved from a Matrix homeserver /sync
  response into the state machine
- you pull requests that you'll need to send back to the homeserver out of the
  state machine

```rust,no_run
use std::collections::BTreeMap;

use matrix_sdk_crypto::{
    DecryptionSettings, EncryptionSyncChanges, OlmError, OlmMachine, TrustRequirement,
};
use ruma::{api::client::sync::sync_events::DeviceLists, device_id, user_id};

#[tokio::main]
async fn main() -> Result<(), OlmError> {
    let alice = user_id!("@alice:example.org");
    let machine = OlmMachine::new(&alice, device_id!("DEVICEID")).await;

    let changed_devices = DeviceLists::default();
    let one_time_key_counts = BTreeMap::default();
    let unused_fallback_keys = Some(Vec::new());
    let next_batch_token = "T0K3N".to_owned();

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    // Push changes that the server sent to us in a sync response.
    let decrypted_to_device = machine
        .receive_sync_changes(
            EncryptionSyncChanges {
                to_device_events: vec![],
                changed_devices: &changed_devices,
                one_time_keys_counts: &one_time_key_counts,
                unused_fallback_keys: unused_fallback_keys.as_deref(),
                next_batch_token: Some(next_batch_token),
            },
            &decryption_settings,
        )
        .await?;

    // Pull requests that we need to send out.
    let outgoing_requests = machine.outgoing_requests().await?;

    // Send the requests out here and call machine.mark_request_as_sent().

    Ok(())
}
```
It is recommended to use the [tutorial] to understand how end-to-end encryption
works in Matrix and how to add end-to-end encryption support in your Matrix
client library.

[Matrix]: https://matrix.org/
[matrix-sdk]: https://github.com/matrix-org/matrix-rust-sdk/

# Crate Feature Flags

The following crate feature flags are available:

| Feature             | Default | Description                                                                                                                |
| ------------------- | :-----: | -------------------------------------------------------------------------------------------------------------------------- |
| `qrcode`            |   No    | Enables QR code based interactive verification                                                                             |
| `js`                |   No    | Enables JavaScript API usage for things like the current system time on WASM (does nothing on other targets)               |
| `testing`           |   No    | Provides facilities and functions for tests, in particular for integration testing store implementations. ATTENTION: do not ever use outside of tests, we do not provide any stability warantees on these, these are merely helpers. If you find you _need_ any function provided here outside of tests, please open a Github Issue and inform us about your use case for us to consider. |

* `testing`: Provides facilities and functions for tests, in particular for integration testing store implementations. ATTENTION: do not ever use outside of tests, we do not provide any stability warantees on these, these are merely helpers. If you find you _need_ any function provided here outside of tests, please open a Github Issue and inform us about your use case for us to consider.

* `_disable-minimum-rotation-period-ms`: Do not use except for testing. Disables the floor on the rotation period of room keys.
# Enabling logging

Users of the `matrix-sdk-crypto` crate can enable log output by depending on the
`tracing-subscriber` crate and including the following line in their
application (e.g. at the start of `main`):

```no_compile
tracing_subscriber::fmt::init();
```

The log output is controlled via the `RUST_LOG` environment variable by
setting it to one of the `error`, `warn`, `info`, `debug` or `trace` levels.
The output is printed to stdout.

The `RUST_LOG` variable also supports a more advanced syntax for filtering
log output more precisely, for instance with crate-level granularity. For
more information on this, check out the [tracing-subscriber documentation].

[tracing-subscriber documentation]: https://docs.rs/tracing-subscriber/latest/tracing_subscriber/
