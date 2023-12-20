# Apple platforms support

This project and build script demonstrate how to create an XCFramework that can be imported into an Xcode project and run on Apple platforms. It can compile and bundle an [entire SDK](#Building-the-SDK), or only a smaller [Crypto module](#Building-only-the-Crypto-SDK) that provides end-to-end encryption for clients that already depend on an SDK (e.g. [Matrix iOS SDK](https://github.com/matrix-org/matrix-ios-sdk))

## Building

### Prerequisites for building universal frameworks

- the Rust toolchain
- Apple targets (e.g. `rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios aarch64-apple-darwin x86_64-apple-darwin`)
- `xcodebuild` command line tool from [Apple](https://developer.apple.com/library/archive/technotes/tn2339/_index.html)
- `lipo` for creating the fat static libs

### Building the SDK

```
cargo xtask swift build-framework
```

The `build-framework` task will go through all the steps required to generate a fully usable `.xcframework`:

1. compile `matrix-sdk-ffi` libraries for iOS, the iOS simulator and macOS under `/target`. Some targets are not part of the standard library and they will be built using the nightly toolchain.
2. `lipo` together the libraries for the same platform under `/generated`
3. run `uniffi` and generate the C header, module map and swift files
4. `xcodebuild` an `xcframework` from the fat static libs and the original iOS one, and add the header and module map to it under `generated/MatrixSDKFFI.xcframework`
5. cleanup and delete the generated files except the .xcframework and the swift sources (that aren't part of the framework)

For development purposes, it will additionally generate a `Package.swift` file in the root of the repo that can be used to add the framework to your project and enable debugging through the use of [rust-xcode-plugin](https://github.com/BrainiumLLC/rust-xcode-plugin) (make sure to run the task with the argument `--profile=reldbg`).

When building the SDK for release you should pass the `--release` argument to the task, which will strip away any symbols and optimise the created binary.

### Building only the Crypto SDK

```
build_crypto_xcframework.sh
```

The `build_crypto_xcframework.sh` script will go through all the steps required to generate a fully usable `.xcframework`:

1. compile `matrix-sdk-crypto-ffi` libraries for iOS and the iOS simulator under `/target`
2. `lipo` together the libraries for the same platform under `/generated`
3. run `uniffi` and generate the C header, module map and swift files
4. `xcodebuild` an `xcframework` from the fat static libs and the original iOS one, and add the header and module map to it under `generated/MatrixSDKCryptoFFI.xcframework`
5. cleanup and delete the generated files except the .xcframework and the swift sources (that aren't part of the framework)

### Building & testing the Swift package

The `Package.swift` file in this directory provides a simple example on how to integrate everything together but also a place to run unit and integration tests from.

It's pre-configured to link to the generated static lib and .swift files so successfully running `cargo xtask swift build-library` first is necessary for it to compile. Afterwards you can execute the tests with `swift test`. Note that for the moment this only works on macOS but we're planning to add Linux support in the future.

## Distribution

The generated framework and Swift code can be distributed and integrated directly but in order to make things simpler we bundle them together as a [Swift package](https://github.com/matrix-org/matrix-rust-components-swift/) in the case of SDK, and as a CocoaPods podspec in the case of Crypto SDK.

### Publishing MatrixSDKCrypto

1. Navigate into `bindings/apple` and run [`build_crypto_xcframework.sh`](#building-only-the-crypto-sdk) script which generates a .zip file with the framework in `./generated` folder

   Note that whilst you can run this command from any git branch, if you want to make a public release, ensure you are building from latest `main`

2. Tag the commit you just used to build the release and push the tag to GitHub

   Use `matrix-sdk-crypto-ffi-<major>.<minor>.<patch>` tag name

3. Create a new [GitHub release](https://github.com/matrix-org/matrix-rust-sdk/releases) using this tag (see [example](https://github.com/matrix-org/matrix-rust-sdk/releases/tag/matrix-sdk-crypto-ffi-0.3.4))
4. Upload the previously generated .zip file to this release
5. Increment the version in [`MatrixSDKCrypto.podspec`](./MatrixSDKCrypto.podspec)

   Note that this is not automated and is a local-only change. To determine the most recent published version, run `pod repo update && pod search MatrixSDKCrypto`
   or check git tags via `git tag | grep matrix-sdk-crypto-ffi`

6. Push new Podspec version to Cocoapods via `pod trunk push MatrixSDKCrypto.podspec --allow-warnings`
