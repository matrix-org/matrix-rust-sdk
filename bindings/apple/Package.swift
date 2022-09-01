// swift-tools-version: 5.6
// The swift-tools-version declares the minimum version of Swift required to build this package.

import PackageDescription

let package = Package(
    name: "MatrixRustSDK",
    platforms: [.iOS(.v15)],
    products: [
        .library(name: "MatrixRustSDK",
                 targets: ["MatrixRustSDK"]),
    ],
    targets: [
        .binaryTarget(name: "MatrixSDKFFI", path: "generated/MatrixSDKFFI.xcframework"),
        .target(name: "MatrixRustSDK",
                dependencies: [.target(name: "MatrixSDKFFI")],
                path: "generated/swift"),
        .testTarget(name: "MatrixRustSDKTests",
                    dependencies: ["MatrixRustSDK"]),
    ]
)
