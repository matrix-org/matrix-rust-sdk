Pod::Spec.new do |s|

    s.name                  = "MatrixSDKCrypto"
    s.version               = "0.1.0"
    s.summary               = "Uniffi based bindings for the Rust SDK crypto crate."
    s.homepage              = "https://github.com/matrix-org/matrix-rust-sdk"
    s.license               = { :type => "Apache License, Version 2.0", :file => "LICENSE" }
    s.author                = { "matrix.org" => "support@matrix.org" }

    s.ios.deployment_target = "11.0"
    s.swift_versions = ['5.0']

    s.source                = { :http => "https://github.com/matrix-org/matrix-rust-sdk/releases/download/matrix-sdk-crypto-ffi-#{s.version}/MatrixSDKCryptoFFI.zip" }
    s.vendored_frameworks   = "MatrixSDKCryptoFFI.xcframework"
    s.source_files          = "Sources/**/*.{swift}"
  
end
