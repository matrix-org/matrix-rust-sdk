# Architecture

The SDK is split into multiple layers:

```
    WASM (external crate matrix-rust-sdk-crypto-wasm)
      /
     /     uniffi
    /     /
   /     bindings (matrix-sdk-ffi)
 crypto   |
bindings  |
   |      |
   |     UI (matrix-sdk-ui)
   |      \
   |       \
   |   main (matrix-sdk)
   | /     /
crypto    /
     \   /
      store (matrix-sdk-base, + all the store impls)
        |
      common (matrix-sdk-common)
```

Where the store implementations are `matrix-sdk-sqlite` and `matrix-sdk-indexeddb` as well as
`MemoryStore` which is defined in `matrix-sdk-base`.

## `crates/matrix-sdk`

This is the main crate, and one that is expected to be used by most consumers. Notable data types
include:

- the `Client`, which can run room-independent requests: logging in/out, creating rooms, running
  sync, etc.
- the `Room`, which represents a room and its state (notably via the observable `RoomInfo`), and
  allows running queries that are room-specific, notably sending events.

## `crates/matrix-sdk-base`

A *sans I/O* crate to represent the base data types persisted in the SDK. No network or storage I/O
happens in this crate, although it defines traits (`StateStore` and `EventCacheStore`) representing
storage backends, as well as dummy in-memory implementations of these traits.

## `crates/matrix-sdk-common`

Common helpers used by most of the other crates; almost a leaf in the dependency tree of our own
crates (the only crate it's using is test helpers).

## `crates/matrix-sdk-crypto`

A *sans I/O* implementation of a state machine that handles end-to-end encryption for Matrix
clients. It defines a `CryptoStore` trait representing storage backends that will perform the
actual storage I/O later, as well as a dummy in-memory implementation of this trait.

## `crates/matrix-sdk-indexeddb`

Implementations of `EventCacheStore`, `StateStore` and `CryptoStore` for a
indexeddb backend (for use in Web browsers, via WebAssembly).

## `crates/matrix-sdk-qrcode`

Implementation of QR codes for interactive verifications, used in the crypto crate.

## `crates/matrix-sdk-sqlite`

Implementations of `EventCacheStore`, `StateStore` and `CryptoStore` for a
SQLite backend.

## `crates/matrix-sdk-store-encryption`

Low-level primitives for encrypting/decrypting/hashing values. Store implementations that
implement encryption at rest can use those primitives.

## `crates/matrix-sdk-ui`

Very high-level primitives implementing the best practices and cutting-edge Matrix tech:

- `EncryptionSyncService`: a specialized service running simplified sliding sync (MSC4186) for
  everything related to crypto and E2EE for the current `Client`.
- `RoomListService`: a specialized service running simplified sliding sync (MSC4186) for
  retrieving the list of current rooms, and exposing its entries.
- `SyncService`: a wrapper for the two previous services, coordinating their running and shutting
  down.
- `Timeline`: a high-level view for a `Room`'s timeline of events, grouping related events
  (aggregations) into single timeline items.

## `bindings/matrix-sdk-crypto-ffi/`

FFI bindings for the crypto crate, used in a Web browser context via WebAssembly. These use
`wasm-bindgen` to generate the bindings. These bindings are used in Element Web and the legacy
Element apps, as of 2024-11-07.

## `bindings/matrix-sdk-ffi/`

FFI bindings for important concepts in `matrix-sdk-ui` and `matrix-sdk`, generated with
[UniFFI](https://github.com/mozilla/uniffi-rs) and to be used from other languages like
Swift/Go/Kotlin. These bindings are used in the ElementX apps, as of 2024-11-07.

## `bindings/matrix-sdk-ffi-macros/`

Macros used in `bindings/matrix-sdk-ffi`.

## `testing/matrix-sdk-test/`

Common test helpers, used by all the other crates.

## `testing/matrix-sdk-test-macros/`

Implementation of the `#[async_test]` test macro.

## `testing/matrix-sdk-integration-testing/`

Fully-fledged integration tests that require spawning a Synapse instance to run. A docker-compose
setup is provided to ease running the tests, and it is compatible for running with Podman too.

# Inspiration

This document has been inspired by the reading of this [blog post](https://matklad.github.io/2021/02/06/ARCHITECTURE.md.html).
