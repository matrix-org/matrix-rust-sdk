## Introduction
**matrix-rust-sdk** leverages [UniFFI](https://mozilla.github.io/uniffi-rs/) to generate bindings for host languages (eg. Swift and Kotlin).

The process requires to developers to write a `.udl` (UniFFI Definition Language) file, where Rust definitions to be exposed are collected.


Rust code related with bindings live in the [matrix-rust-sdk/bindings](https://github.com/matrix-org/matrix-rust-sdk/tree/main/bindings) folder.

Each Rust crate can expose definitions to UniFFI using two different approaches:
- Using an `.udl` file. When a crate has one, you find it under the `src` folder (an example is [here](https://github.com/matrix-org/matrix-rust-sdk/blob/main/bindings/matrix-sdk-ffi/src/api.udl)). In this case most of the Rust definitions can be found in the file.
- Add UniFFI directivies as Rust attributes. In this case Rust source files (`.rs`) contain attributes related to UniFFI (e.g. `#[uniffi::export]`). 
 

## Expose Rust methods/types to UniFFI

### Check if the API is already on UniFFI

First of all check if the API / type you are looking for exists on UniFFI already. Most of exposed matrix definitions are collected in the crate [matrix-rust-sdk](https://github.com/matrix-org/matrix-rust-sdk/tree/main/bindings/matrix-sdk-ffi).
This crate contains mainly small Rust wrappers around the actual Rust SDK (e.g. the crate [matrix-sdk](https://github.com/matrix-org/matrix-rust-sdk/tree/main/crates/matrix-sdk))

If the API is on UniFFI already, you can probably see it also in the file [matrix-sdk-ffi/src/api.udl](https://github.com/matrix-org/matrix-rust-sdk/blob/main/bindings/matrix-sdk-ffi/src/api.udl) where most of the matrix definitions are declared.


### Adding a missing matrix API

1. Unless you want to contribute on the crypto side, you probably need to add some code in the [matrix-rust-sdk/bindings/matrix-sdk-ffi](https://github.com/matrix-org/matrix-rust-sdk/tree/main/bindings/matrix-sdk-ffi) crate. After you find the crate you need to understand which file is best to contain the new Rust definition. When exposing new matrix API often (but not always) you need to touch the file [client.rs](https://github.com/matrix-org/matrix-rust-sdk/blob/main/bindings/matrix-sdk-ffi/src/client.rs).

2. Identify the API to expose in the the target Rust crate (typically in [matrix-sdk](https://github.com/matrix-org/matrix-rust-sdk/tree/main/crates/matrix-sdk). If you can’t find your definition. If you can’t find it, you probably need to touch the actual Rust sdk as well. In this case you typically just need to write some around [ruma](https://github.com/ruma/ruma) (a rust sdk’s dependency) which already implements most of the matrix protocol. 

3. When you got the Rust code you need to expose to UniFFI, just write a small Rust wrapper in the related UniFFI crate (most of the time is **matrix-sdk-ffi**).

4. When your new definition is ready, remember to expose it in the related `.udl` file (or add an appropriate UniFFI export Rust attribute). For the **matrix-sdk-ffi** crate the definition file is [api.udl](https://github.com/matrix-org/matrix-rust-sdk/blob/main/bindings/matrix-sdk-ffi/src/api.udl). Remember: the language inside a `.udl` file isn’t Rust. To learn more about how map Rust into UDL read [here](https://mozilla.github.io/uniffi-rs/udl_file_spec.html).

## FAQ

**Q**: I wrote my Rust code and exposed it to UniFFI. How can I check if the compiler is happy?\
**A**: Run `cargo build` in the crate you touched (e.g. matrix-sdk-ffi). The compiler will complain if the Rust code and/or the `.udl` is wrong.


**Q**: The compiler is happy with my code but the CI is failing on GitHub. How can I fix it?\
**A**: The CI may fail for different reasons, you need to have a look on the failing GitHub workflow. One common reason though, is that the linter ([Clippy](https://github.com/rust-lang/rust-clippy)) isn’t happy with your code. If this is the case, you can run `cargo clippy` in the crate you touched to see what’s wrong.