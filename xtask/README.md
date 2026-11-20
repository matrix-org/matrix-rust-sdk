# Martrix Rust SDK xtask

Helper and make utility for testing and building matrix-rust-sdk FFI

## Usage

see `cargo xtask --help`.

## Reusing

If you are building a ffi project on top of the matrix-rust-sdk-ffi, you might want to reuse this utility
to build your libraries, too. To do so, add a minimal xtask-bin-crate to your project, which depends
on this crate via a git-dependency. 

In its `main.rs`, add `use xtask::*;` to make the `cargo xtask` commands available.

### Custom Configuration

If you are building your own project on top of the matrix-rust-sdk-ffi, you probably need to change
some configuration this tool uses to identifty which crate to build andwhich features to activate.
You can do that by setting the following parameters in your *workspace* `Cargo.toml` file:

```toml
[workspace.metadata.xtask] # this is a new section you need to add
# this is the main crate that exposes the FFI
ffi_crate = "my_custom_ffi"
# these are the features to activate when building the kotlin bindings, comma-separate single strings
kotlin_features = "kotlin1,kotlin-extra"
# these are the features to activate when building the swift bindings, comma-separate single strings
swift_features = "apple,secure-key"
```
