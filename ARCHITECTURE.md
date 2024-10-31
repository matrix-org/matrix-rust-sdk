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

Where the store impls are `matrix-sdk-sqlite` and `matrix-sdk-indexeddb` as well
as `MemoryStore` which is defined in `matrix-sdk-base`.

## `crates/matrix-sdk`
## `crates/matrix-sdk-base`
## `crates/matrix-sdk-common`
## `crates/matrix-sdk-crypto`
## `crates/matrix-sdk-indexeddb`

Implementations of `EventCacheStore`, `StateStore` and `CryptoStore` for a
indexeddb backend (for use in Web browsers).

## `crates/matrix-sdk-qrcode`

Implementation of QR codes for interactive verifications, used in the crypto crate.

## `crates/matrix-sdk-sqlite`

Implementations of `EventCacheStore`, `StateStore` and `CryptoStore` for a
SQLite backend.

## `crates/matrix-sdk-store-encryption`

Low-level primitives for encrypting/decrypting/hashing values. Store implementations that
implement encryption at rest can use those primitives.

## `crates/matrix-sdk-ui`

## `bindings/matrix-sdk-crypto-ffi/`
## `bindings/matrix-sdk-ffi/`
## `bindings/matrix-sdk-ffi-macros/`

## `testing/matrix-sdk-test/`
## `testing/matrix-sdk-test-macros/`
## `testing/matrix-sdk-integration-testing/`

# Inspiration

This document has been inspired, 
