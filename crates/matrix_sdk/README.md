A [Matrix](https://matrix.org/) client library written in Rust.

The matrix-sdk aims to be a general purpose client library for writing Matrix
clients, bots, and other Matrix related things that use the client-server API to
communicate with a Matrix homeserver.

# Examples
Connecting and logging in to a homeserver is pretty starightforward:

```rust,no_run
use std::convert::TryFrom;
use matrix_sdk::{
    Client, SyncSettings, Result,
    ruma::{UserId, events::{SyncMessageEvent, room::message::MessageEventContent}},
};

#[tokio::main]
async fn main() -> Result<()> {
    let alice = UserId::try_from("@alice:example.org")?;
    let client = Client::new_from_user_id(alice.clone()).await?;

    // First we need to log in.
    client.login(alice.localpart(), "password", None, None).await?;

    client
        .register_event_handler(
            |ev: SyncMessageEvent<MessageEventContent>| async move {
                println!("Received a message {:?}", ev);
            },
        )
        .await;

    // Syncing is important to synchronize the client state with the server.
    // This method will never return.
    client.sync(SyncSettings::default()).await;

    Ok(())
}
```

More examples can be found in the [examples] directory.

# Crate Feature Flags

The following crate feature flags are available:

* `encryption`: Enables end-to-end encryption support in the library.
* `qrcode`: Enables qrcode verification support in the library. This will also
  enable `encryption`. Enabled by default.
* `sled_cryptostore`: Enables a Sled based store for the encryption keys. If
  this is disabled and `encryption` support is enabled the keys will by
  default be stored only in memory and thus lost after the client is
  destroyed.
* `markdown`: Support for sending markdown formatted messages.
* `socks`: Enables SOCKS support in reqwest, the default HTTP client.
* `sso_login`: Enables SSO login with a local http server.
* `require_auth_for_profile_requests`: Whether to send the access token in
  the authentication header when calling endpoints that retrieve profile
  data. This matches the synapse configuration
  `require_auth_for_profile_requests`. Enabled by default.
* `appservice`: Enables low-level appservice functionality. For an
  high-level API there's the `matrix-sdk-appservice` crate
* `anyhow`: Support for returning `anyhow::Result<()>` from event handlers.

# Enabling logging

Users of the matrix-sdk crate can enable log output by depending on the
`tracing-subscriber` crate and including the following line in their
application (e.g. at the start of `main`):

```rust
tracing_subscriber::fmt::init();
```

The log output is controlled via the `RUST_LOG` environment variable by
setting it to one of the `error`, `warn`, `info`, `debug` or `trace` levels.
The output is printed to stdout.

The `RUST_LOG` variable also supports a more advanced syntax for filtering
log output more precisely, for instance with crate-level granularity. For
more information on this, check out the [tracing_subscriber documentation].

[examples]: https://github.com/matrix-org/matrix-rust-sdk/tree/master/crates/matrix_sdk/examples
[tracing_subscriber documentation]: https://tracing.rs/tracing_subscriber/filter/struct.envfilter
