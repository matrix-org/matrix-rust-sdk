// swift-tools-version:5.6

// A package manifest for local development. This file will be copied
// into the root of the repo when generating an XCFramework.

import PackageDescription

let package = Package(
    name: "MatrixRustSDK",
    platforms: [
        .iOS(.v15),
        .macOS(.v12)
    ],
    products: [
        .library(name: "MatrixRustSDK",
                 type: .dynamic,
                 targets: ["MatrixRustSDK"]),
    ],
    targets: [
        .binaryTarget(name: "MatrixSDKFFI", path: "bindings/apple/generated/MatrixSDKFFI.xcframework"),
        .target(name: "MatrixRustSDK",
                dependencies: [.target(name: "MatrixSDKFFI")],
                path: "bindings/apple/generated/swift")
    ]
)
