# Matrix-Rust-SDK Node.js Bindings

## 0.1.0-beta.0 - 2022-07-21

Welcome to the first release of `matrix-sdk-crypto-nodejs`. This is a
Node.js binding for the Rust `matrix-sdk-crypto` library. This is a
no-network-IO implementation of a state machine, named `OlmMachine`,
that handles E2EE (End-to-End Encryption) for Matrix clients.

The goal of this binding is _not_ to cover the entirety of the
`matrix-sdk-crypto` API, but only what's required to build Matrix bots
or Matrix bridges (i.e. to connect different networks together via the
Matrix protocol).

This project replaces and deprecates a previous project, with the same
name and same goals, inside [the `matrix-rust-sdk-bindings`
repository](https://github.com/matrix-org/matrix-rust-sdk-bindings),
with the NPM package name `@turt2live/matrix-sdk-crypto-nodejs`. The
The new official package name is
`@matrix-org/matrix-sdk-crypto-nodejs`.

Note: All bindings are now part of [the `matrix-rust-sdk`
repository](https://github.com/matrix-org/matrix-rust-sdk) (see the
`bindings/` root directory).

[A documentation is available inside the new
`matrix-sdk-crypto-nodejs`
project](https://github.com/matrix-org/matrix-rust-sdk/tree/0bde5ccf38f8cda3865297a2d12ddcdaf4b80ca7/bindings/matrix-sdk-crypto-nodejs).
