# Matrix rust components kotlin

This project and build scripts demonstrate how to create an aar and how to import it in your android projects.

## Prerequisites

* the Rust toolchain
* cargo-ndk < 2.12.0 `cargo install cargo-ndk --version 2.11.0`
* android targets (e.g. `rustup target add \
  aarch64-linux-android \
  armv7-linux-androideabi \
  x86_64-linux-android \
  i686-linux-android`)

## Building the SDK

To build the full sdk and get an aar you can call :
`./bindings/kotlin/scripts/build_sdk.sh /matrix-rust_sdk/bindings/kotlin/sample/libs`
where the parameter is the path for the aar to go

## License

[Apache-2.0](https://www.apache.org/licenses/LICENSE-2.0)
