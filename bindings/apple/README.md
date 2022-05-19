# Apple platforms support

This project and build script demonstrate how to create an XCFramework that can be imported into an Xcode project and run on Apple platforms.

## Building the universal framework

```
sh build_xcframework.sh
```

**Prerequisites**

* the Rust toolchain
* UniFFI - `cargo install uniffi_bindge`
* Apple targets (e.g. `rustup target add aarch64-apple-ios`)
* `xcodebuild` command line tool from [Apple](https://developer.apple.com/library/archive/technotes/tn2339/_index.html)
* `lipo` for creating the fat static libs


The `build_xcframework.sh` script will go through all the steps required to generate a fully usable `.xcframework`:

1. compile `matrix-sdk-ffi` libraries for iOS, the iOS simulator, MacOS, and Mac Catalyst under `/target`. Some targets are not part of the standard library and they will be built using the nightly toolchain. 
* `lipo` together the libraries for the same platform under `/generated`
* run `uniffi` and generate the C header, module map and swift files
* `xcodebuild` an `xcframework` from the fat static libs and the original iOS one, and add the header and module map to it under `generated/MatrixSDKFFI.xcframework`
* cleanup and delete the generated files except the .xcframework and the swift sources (that aren't part of the framework)

## Running the Xcode project

The Xcode project is meant to provide a simple example on how to integrate everything together but also a place to run unit and integration tests from.

It's pre-configured to link to the generated .xcframework and .swift files so successfully running the script first is necessary for it to compile.

It makes the compiled code available to swift by importing the C header through its bridging header.

Once all the generated components are available running it should be as easy as choosing a platform and clicking run.

## Distribution
The generated framework and Swift code can be distributed and integrated directly but in order to make things simpler we bundle them together as a Swift package available [TBD](here)