A high-level, batteries-included [Matrix](https://matrix.org/) client library
written in Rust.

This crate seeks to be a general-purpose library for writing software using the
Matrix [Client-Server API](https://matrix.org/docs/spec/client_server/latest)
to communicate with a Matrix homeserver. If you're writing a typical Matrix
client or bot, this is likely the crate you need.

However, the crate is designed in a modular way and depends on several
other lower-level crates. If you're attempting something more custom, you might be interested in these:

- [`matrix_sdk_base`]: A no-network-IO client state machine which can be used
  to embed a Matrix client into an existing network stack or to build a new
  Matrix client library on top.
- [`matrix_sdk_crypto`](https://docs.rs/matrix-sdk-crypto/*/matrix_sdk_crypto/):
  A no-network-IO encryption state machine which can be used to add Matrix E2EE
  support into an existing client or library.

# Getting started

The central component you'll be interacting with is the [`Client`]. A basic use
case will include instantiating the client, logging in as a user, registering
some event handlers and then syncing.

This is demonstrated in the example below.

```rust,no_run
use std::convert::TryFrom;
use matrix_sdk::{
    Client, config::SyncSettings,
    ruma::{user_id, events::room::message::SyncRoomMessageEvent},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let alice = user_id!("@alice:example.org");
    let client = Client::builder().user_id(alice).build().await?;

    // First we need to log in.
    client.login(alice, "password", None, None).await?;

    client
        .register_event_handler(|ev: SyncRoomMessageEvent| async move {
            println!("Received a message {:?}", ev);
        })
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

| Feature            | Default | Description                                                           |
| ------------------ | :-----: | --------------------------------------------------------------------- |
| `anyhow`           |   No    | Better logging for event handlers that return `anyhow::Result`        |
| `encryption`       |   Yes   | End-to-end encryption support                                         |
| `eyre`             |   No    | Better logging for event handlers that return `eyre::Result`          |
| `image_proc`       |   No    | Enables image processing to generate thumbnails                       |
| `image_rayon`      |   No    | Enables faster image processing                                       |
| `markdown`         |   No    | Support to send Markdown-formatted messages                           |
| `qrcode`           |   Yes   | QR code verification support                                          |
| `sled_cryptostore` |   Yes   | Persistent storage for E2EE related data                              |
| `sled_statestore`  |   No    | Persistent storage of state data with sled                            |
| `socks`            |   No    | Enables SOCKS support in the default HTTP client, [`reqwest`]         |
| `sso_login`        |   No    | Enables SSO login with a local HTTP server                            |
| `indexedb_stores`  |   No    | Persistent storage of state & e2ee data with indexeddb for web/wasm32 |

[`reqwest`]: https://docs.rs/reqwest/0.11.5/reqwest/index.html

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

[examples]: https://github.com/matrix-org/matrix-rust-sdk/tree/main/crates/matrix-sdk/examples
[tracing_subscriber documentation]: https://tracing.rs/tracing_subscriber/filter/struct.envfilter
