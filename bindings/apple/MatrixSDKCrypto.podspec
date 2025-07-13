Pod::Spec.new do |s|

    s.name                  = "MatrixSDKCrypto"
    s.version               = "0.4.1" # Version is only incremented manually and locally before pushing to CocoaPods
    s.summary               = "Uniffi based bindings for the Rust SDK crypto crate."
    s.homepage              = "https://github.com/matrix-org/matrix-rust-sdk"
    s.license               = { :type => "Apache License, Version 2.0", :file => "LICENSE" }
    s.author                = { "matrix.org" => "support@matrix.org" }

    s.ios.deployment_target = "13.0"
    s.osx.deployment_target = "10.15"

    s.swift_versions = ['5.1', '5.2']

    s.source                = { :http => "https://github.com/matrix-org/matrix-rust-sdk/releases/download/matrix-sdk-crypto-ffi-#{s.version}/MatrixSDKCryptoFFI.zip" }
    s.vendored_frameworks   = "MatrixSDKCryptoFFI.xcframework"
    s.source_files          = "Sources/**/*.{swift}"
  
end
