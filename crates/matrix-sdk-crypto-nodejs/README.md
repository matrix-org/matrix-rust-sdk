# `matrix-sdk-crypto-nodejs`

This is the NodeJS bindings to [`matrix-sdk-crypto`].

## Installation

```sh
$ npm install
$ npm run build
$ npm run test
```

### With tracing

If you want to get [tracing](https://tracing.rs) (to get logs):

```sh
$ npm run build -- --features tracing
$ RUST_LOG=debug npm run test
```

See
[`tracing-subscriber`](https://tracing.rs/tracing_subscriber/index.html)
to learn more about the `RUST_LOG` environment variable.




[`matrix-sdk-crypto`]: https://github.com/matrix-org/matrix-rust-sdk/tree/main/crates/matrix-sdk-crypto
