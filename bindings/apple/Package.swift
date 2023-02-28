// swift-tools-version: 5.6
// The swift-tools-version declares the minimum version of Swift required to build this package.

import PackageDescription

let package = Package(
    name: "MatrixRustSDK",
    platforms: [
        .iOS(.v15),
        .macOS(.v12)
    ],
    products: [
        .library(name: "MatrixRustSDK",
                 targets: ["MatrixRustSDK"]),
    ],
    targets: [
        .target(name: "MatrixRustSDK",
                path: "generated/swift",
                swiftSettings: [
                    .unsafeFlags(["-I", "./generated/matrix_sdk_ffi"])
                ]),
        .testTarget(name: "MatrixRustSDKTests",
                    dependencies: ["MatrixRustSDK"],
                    swiftSettings: [
                        .unsafeFlags(["-I", "./generated/matrix_sdk_ffi"])
                    ],
                    linkerSettings: [
                        .linkedLibrary("matrix_sdk_ffi", .when(platforms: [.macOS])),
                        .linkedLibrary("matrix_sdk_ffiFFI", .when(platforms: [.linux])),
                        .unsafeFlags(["-L./generated/matrix_sdk_ffi"])
                    ])
    ]
)
