# FFI bindings for the rust matrix SDK

This uses [`uniffi`](https://mozilla.github.io/uniffi-rs/Overview.html) to build
the matrix bindings for native support and wasm-bindgen for web-browser assembly
support. Please refer to the specific section to figure out how to build and use
the bindings for your platform.

## Features

Given the number of platforms targeted, we have broken out a number of features

### Functionality

- `sentry`: Enable error monitoring using Sentry, not supports on Wasm
  platforms.
- `sqlite`: Use SQLite for the session storage.
- `bundled-sqlite`: Use an embedded version of SQLite instead of the system
  provided one.
- `indexeddb`: Use IndexedDB for the session storage.

### Unstable specs

- `unstable-msc4274`: Adds support for gallery message types, which contain
  multiple media elements.

## Platforms

Each supported target should use features to build the relevant system. Here are
some suggested feature flags for the major platforms:

- Android: `"bundled-sqlite,unstable-msc4274,sentry"`
- iOS: `"bundled-sqlite,unstable-msc4274,sentry"`
- JavaScript/Wasm: `"indexeddb,unstable-msc4274"`

### Swift/iOS sync

TBD

## Extending the FFI

This FFI is not publsihed yet, but you can build can extend it by depending on
this crate via a git dependency. To be able to get access to the actual inner/
rust types, make sure to enabled the experimental `experimental-preludes` feature:
```toml
[dependencies]
matrix-sdk-ffi = { git = "https://github.com/matrix-org/matrix-rust-sdk", features = ["experimental-preludes"] }
```

Once you have that you can use the `matrix_sdk_ffi::prelude::Client::cloned_inner` of
the FFIClient to get access to the inner `matrix_sdk::Client` type. An example for an
example wrapper:

```rust
use matrix_sdk_ffi::prelude::Client;

#[derive(Debug, Clone, uniffi::Object)]
struct FooService {
    // our new wrapper type for the custom FFI
    inner: matrix_sdk::Client,
}

#[matrix_sdk_ffi_macros::export]
impl FooService {
    pub fn is_connected(&self) -> bool {
        self.inner.is_active()
    }
    
    // -- custom code that has access to
    // the inner `matrix_sdk::Client` type
    // and can now be accessed via FFI
}


// The FFI function to call to make a `FooService` instance
// from an existing FFI-Client
#[matrix_sdk_ffi_macros::export]
pub fn foo_service(
    client: &Client,
) -> FooService {
    FooService {
        inner: client.cloned_inner(),
    }
}

```

Don't forget to add the feature flag when building the cargo docs to see all the available types
and functions.
