#!/usr/bin/env bash
set -eEu

cd "$(dirname "$0")"

IS_CI=false

if [ $# -eq 1 ]; then
  IS_CI=true
  echo "Running CI build"
else
  echo "Running debug build"
fi

# Path to the repo root
SRC_ROOT=..

TARGET_DIR="${SRC_ROOT}/target"

GENERATED_DIR="${SRC_ROOT}/generated"
mkdir -p ${GENERATED_DIR}

# Release for now. Debug builds cause crashes deep inside the Tokio runtime.
REL_FLAG="--release"
REL_TYPE_DIR="release"

# iOS Simulator
cargo build -p matrix-sdk-ffi ${REL_FLAG} --target "aarch64-apple-ios-sim"
cargo build -p matrix-sdk-ffi ${REL_FLAG} --target "x86_64-apple-ios"

lipo -create \
  "${TARGET_DIR}/x86_64-apple-ios/${REL_TYPE_DIR}/libmatrix_sdk_ffi.a" \
  "${TARGET_DIR}/aarch64-apple-ios-sim/${REL_TYPE_DIR}/libmatrix_sdk_ffi.a" \
  -output "${GENERATED_DIR}/libmatrix_sdk_ffi_iossimulator.a"

# Generate uniffi files
uniffi-bindgen generate "${SRC_ROOT}/crates/matrix-sdk-ffi/src/api.udl" --language swift --out-dir ${GENERATED_DIR}

# Move them to the right place
HEADERS_DIR=${GENERATED_DIR}/headers
mkdir -p ${HEADERS_DIR}

mv ${GENERATED_DIR}/*.h ${GENERATED_DIR}/*.modulemap ${HEADERS_DIR}

SWIFT_DIR="${GENERATED_DIR}/swift"
mkdir -p ${SWIFT_DIR}

mv ${GENERATED_DIR}/*.swift ${SWIFT_DIR}

# Build the xcframework

if [ -d "${GENERATED_DIR}/MatrixSDKFFI.xcframework" ]; then rm -rf "${GENERATED_DIR}/MatrixSDKFFI.xcframework"; fi

xcodebuild -create-xcframework \
  -library "${GENERATED_DIR}/libmatrix_sdk_ffi_iossimulator.a" \
  -headers ${HEADERS_DIR} \
  -output "${GENERATED_DIR}/MatrixSDKFFI.xcframework"

# Cleanup

# if [ -f "${GENERATED_DIR}/libmatrix_sdk_ffi_iossimulator.a" ]; then rm -rf "${GENERATED_DIR}/libmatrix_sdk_ffi_iossimulator.a"; fi
# if [ -d ${HEADERS_DIR} ]; then rm -rf ${HEADERS_DIR}; fi

if [ "$IS_CI" = false ] ; then
  echo "Preparing matrix-rust-components-swift"

  # Debug -> Copy generated files over to ../../../matrix-rust-components-swift
  echo "$(echo "import MatrixSDKFFIWrapper\n"; cat "${SWIFT_DIR}/sdk.swift")" > "${SWIFT_DIR}/sdk.swift"

  rsync -a --delete "${GENERATED_DIR}/MatrixSDKFFI.xcframework" "${SRC_ROOT}/../matrix-rust-components-swift/"
  rsync -a --delete "${GENERATED_DIR}/swift/" "${SRC_ROOT}/../matrix-rust-components-swift/Sources/MatrixRustSDK"
fi
