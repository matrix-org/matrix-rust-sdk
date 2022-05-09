# matrix-sdk-sled

This crate implements a storage backend using sled for native and mobile environments using the matrix-sdk-base primitives. When using **matrix-sdk** this is included by default.


## Crate Feature Flags

The following crate feature flags are available:

* `state-store`: (on by default) Enables the state store
* `crypto-store`: Enables the store for end-to-end encrypted data.
* `experimental-timeline`: implements the new experimental timeline interfaces


## Minimum Supported Rust Version (MSRV)

These crates are built with the Rust language version 2021 and require a minimum compiler version of `1.60`

## License

[Apache-2.0](https://www.apache.org/licenses/LICENSE-2.0)