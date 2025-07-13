# Matrix Rust SDK bindings

In this directory, one can find bindings to the Rust SDK that are
maintained by the owners of the Matrix Rust SDK project.

* [`apple`] or `matrix-rust-components-swift`, Swift bindings of the
  [`matrix-sdk`] crate via [`matrix-sdk-ffi`],
* [`matrix-sdk-crypto-ffi`], UniFFI (Kotlin, Swift, Python, Ruby) bindings of the [`matrix-sdk-crypto`]
  crate,
* [`matrix-sdk-ffi`], UniFFI bindings of the [`matrix-sdk`] crate.

There are also external bindings in other repositories:

* [`matrix-sdk-crypto-wasm`], JavaScript / WebAssembly bindings of the
  [`matrix-sdk-crypto`] crate,
* [`matrix-sdk-crypto-nodejs`], Node.js bindings of the
  [`matrix-sdk-crypto`] crate

[`apple`]: ./apple
[`matrix-sdk-crypto-ffi`]: ./matrix-sdk-crypto-ffi
[`matrix-sdk-crypto`]: ../crates/matrix-sdk-crypto
[`matrix-sdk-ffi`]: ./matrix-sdk-ffi
[`matrix-sdk`]: ../crates/matrix-sdk

[`matrix-sdk-crypto-wasm`]: https://github.com/matrix-org/matrix-rust-sdk-crypto-wasm
[`matrix-sdk-crypto-nodejs`]: https://github.com/matrix-org/matrix-rust-sdk-crypto-nodejs

## Contributing

To contribute read this [guide](./CONTRIBUTING.md).
