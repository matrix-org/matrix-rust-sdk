# matrix-sdk-sled

This crate implements a storage backend using [sled][sled] for native and mobile environments using the matrix-sdk-base primitives. When using **matrix-sdk** this is included by default.

_Note_: the future of [sled][sled] is unclear. While it is currently the default for mobile and native environments for matrix-rust-sdk, [we are actively looking at replacing it with a different storage backend](https://github.com/matrix-org/matrix-rust-sdk/issues/294).


## Crate Feature Flags

The following crate feature flags are available:

* `state-store`: (on by default) Enables the state store
* `crypto-store`: Enables the store for end-to-end encrypted data.
* `experimental-timeline`: implements the new experimental timeline interfaces


## Minimum Supported Rust Version (MSRV)

These crates are built with the Rust language version 2021 and require a minimum compiler version of `1.60`.

## License

[Apache-2.0](https://www.apache.org/licenses/LICENSE-2.0)


[sled]: https://sled.rs/