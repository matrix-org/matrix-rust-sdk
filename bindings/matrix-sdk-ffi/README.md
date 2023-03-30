# FFI bindings for the rust matrix SDK

This uses [´uniffi´](https://mozilla.github.io/uniffi-rs/Overview.html) to build the matrix bindings for native support and wasm-bindgen for web-browser assembly support. Please refer to the specific section to figure out how to build and use the bindings for your platform.

# OpenTelemetry support

The bindings have support for OpenTelemetry, allowing for the upload of traces to
any OpenTelemetry collector. This support is provided through the
[`opentelemetry`](https://docs.rs/opentelemetry/latest/opentelemetry/)
crate, which requires the [`protoc`] binary to be installed during the build
process.

## Platforms

### Swift/iOS sync



### Swift/iOS async

TBD
