# Podspec file for local-development only, which points to local version of generated framework instead of github release
# To use this, the file needs to be copied to the root of `matrix-rust-sdk` in order to have access to the source files.`
Pod::Spec.new do |s|

    s.name                  = "MatrixSDKCrypto"
    s.version               = "0.4.1"
    s.summary               = "Uniffi based bindings for the Rust SDK crypto crate."
    s.homepage              = "https://github.com/matrix-org/matrix-rust-sdk"
    s.license               = { :type => "Apache License, Version 2.0", :file => "LICENSE" }
    s.author                = { "matrix.org" => "support@matrix.org" }
 
    s.ios.deployment_target = "13.0"
    s.osx.deployment_target = "10.15"

    s.swift_versions = ['5.1', '5.2']

    s.source                = { :git => "Not Published", :tag => "Cocoapods/#{s.name}/#{s.version}" }
    s.vendored_frameworks   = "generated/MatrixSDKCryptoFFI.xcframework"
    s.source_files          = "generated/Sources/**/*.{swift}"
    s.resources             = ["bindings/matrix-sdk-crypto-ffi/src/**/*.rs", "crates/matrix-sdk-crypto/src/**/*.rs"]
end
