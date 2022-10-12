# Apple platforms support

This project and build script demonstrate how to create an XCFramework that can be imported into an Xcode project and run on Apple platforms. It can compile and bundle an [entire SDK](#Building-the-SDK), or only a smaller [Crypto module](#Building-only-the-Crypto-SDK) that provides end-to-end encryption for clients that already depend on an SDK (e.g. [Matrix iOS SDK](https://github.com/matrix-org/matrix-ios-sdk))

## Prerequisites for building universal frameworks

* the Rust toolchain
* UniFFI - `cargo install uniffi_bindgen`
* Apple targets (e.g. `rustup target add aarch64-apple-ios`)
* `xcodebuild` command line tool from [Apple](https://developer.apple.com/library/archive/technotes/tn2339/_index.html)
* `lipo` for creating the fat static libs

## Building the SDK

```
sh build_xcframework.sh
```

The `build_xcframework.sh` script will go through all the steps required to generate a fully usable `.xcframework`:

1. compile `matrix-sdk-ffi` libraries for iOS, the iOS simulator, MacOS, and Mac Catalyst under `/target`. Some targets are not part of the standard library and they will be built using the nightly toolchain. 
2. `lipo` together the libraries for the same platform under `/generated`
3. run `uniffi` and generate the C header, module map and swift files
4. `xcodebuild` an `xcframework` from the fat static libs and the original iOS one, and add the header and module map to it under `generated/MatrixSDKFFI.xcframework`
5. cleanup and delete the generated files except the .xcframework and the swift sources (that aren't part of the framework)

## Building only the Crypto SDK

```
build_crypto_xcframework.sh
```

The `build_crypto_xcframework.sh` script will go through all the steps required to generate a fully usable `.xcframework`:

1. compile `matrix-sdk-crypto-ffi` libraries for iOS and the iOS simulator under `/target`
2. `lipo` together the libraries for the same platform under `/generated`
3. run `uniffi` and generate the C header, module map and swift files
4. `xcodebuild` an `xcframework` from the fat static libs and the original iOS one, and add the header and module map to it under `generated/MatrixSDKCryptoFFI.xcframework`
5. cleanup and delete the generated files except the .xcframework and the swift sources (that aren't part of the framework)

## Running the Xcode project

The Xcode project is meant to provide a simple example on how to integrate everything together but also a place to run unit and integration tests from.

It's pre-configured to link to the generated .xcframework and .swift files so successfully running the script first is necessary for it to compile.

It makes the compiled code available to swift by importing the C header through its bridging header.

Once all the generated components are available running it should be as easy as choosing a platform and clicking run.

## Distribution

The generated framework and Swift code can be distributed and integrated directly but in order to make things simpler we bundle them together as a Swift package available [TBD](here) in the case of SDK, and as CocoaPods podspec in the case of Crypto SDK.

### Publishing MatrixSDKCrypto
1. Run `build_crypto_xcframework.sh` script which generates a .zip file with the framework
2. Increment the version in `MatrixSDKCrypto.podspec`
3. Create a new [GitHub release](https://github.com/matrix-org/matrix-rust-sdk/releases) with the same version (see [example](https://github.com/matrix-org/matrix-rust-sdk/releases/tag/matrix-sdk-crypto-ffi-0.1.0) for naming)
4. Upload the .zip file to this release
5. Push new Podspec version to Cocoapods via `pod trunk push MatrixSDKCrypto.podspec --allow-warnings`
