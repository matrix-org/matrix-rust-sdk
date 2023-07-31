A no-network-IO implementation of a state machine that handles end-to-end
encryption for [Matrix] clients.

If you're just trying to write a Matrix client or bot in Rust, you're probably
looking for [matrix-sdk] instead.

However, if you're looking to add end-to-end encryption to an existing Matrix
client or library, read on.

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

# Enabling logging

Users of the `matrix-sdk-crypto` crate can enable log output by depending on the
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
more information on this, check out the [tracing-subscriber documentation].

[examples]: https://github.com/matrix-org/matrix-rust-sdk/tree/main/crates/matrix-sdk/examples
[tracing-subscriber documentation]: https://docs.rs/tracing-subscriber/latest/tracing_subscriber/
