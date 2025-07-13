# matrix-sdk-indexedddb

This crate implements a storage backend on IndexedDB for web environments using the matrix-sdk-base primitives. 

## Usage

The most common usage pattern would be to have this included via `matrix-sdk` in your `Cargo.toml` and leave
instantiation to it.

```toml,no_test
[target.'cfg(target_family = "wasm")'.dependencies]
matrix-sdk = { version = "0.5, default-features = false, features = ["indexeddb", "e2e-encryption"] }
```


## Crate Feature Flags

The following crate feature flags are available:

* `e2e-encryption`: (on by default) Enables the store for end-to-end encrypted data (`IndexeddbCryptoStore`).
* `state-store`: (on by default) Enables the `StateStore` implementation (`IndexeddbStateStore`).
