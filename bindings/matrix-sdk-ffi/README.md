# FFI bindings for the rust matrix SDK

This uses [`uniffi`](https://mozilla.github.io/uniffi-rs/Overview.html) to build the matrix bindings for native support and wasm-bindgen for web-browser assembly support. Please refer to the specific section to figure out how to build and use the bindings for your platform.

## Features

Given the number of platforms targeted, we have broken out a number of features

### Platform specific

- `rustls-tls`: Use Rustls as the TLS implementation, necessary on Android platforms.
- `native-tls`: Use the TLS implementation provided by the host system, necessary on iOS and Wasm platforms.

### Functionality

- `sentry`: Enable error monitoring using Sentry, not supports on Wasm platforms.
- `sqlite`: Use SQLite for the session storage.
- `bundled-sqlite`: Use an embedded version of SQLite instead of the system provided one.
- `indexeddb`: Use IndexedDB for the session storage.

### Unstable specs

- `unstable-msc4274`: Adds support for gallery message types, which contain multiple media elements.

## Platforms

Each supported target should use features to select the relevant TLS system. Here are some suggested feature flags for the major platforms:

- Android: `"bundled-sqlite,unstable-msc4274,rustls-tls,sentry"`
- iOS: `"bundled-sqlite,unstable-msc4274,native-tls,sentry"`
- JavaScript/Wasm: `"indexeddb,unstable-msc4274,native-tls"`

### Swift/iOS sync

TBD
