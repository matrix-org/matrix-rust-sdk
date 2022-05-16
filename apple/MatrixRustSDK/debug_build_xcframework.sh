#!/usr/bin/env bash
set -eEu

cd "$(dirname "$0")"

# Path to the repo root
SRC_ROOT=../..

TARGET_DIR="${SRC_ROOT}/target"

GENERATED_DIR="${SRC_ROOT}/generated"
mkdir -p ${GENERATED_DIR}

# Release for now. Debug builds cause crashes deep inside the Tokio runtime.
REL_FLAG="--release"
REL_TYPE_DIR="release"

# Build static the lib for aarch64 simulator

cargo +nightly build --locked -p matrix-sdk-ffi ${REL_FLAG} --target "aarch64-apple-ios-sim"

lipo -create \
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

if [ -f "${GENERATED_DIR}/libmatrix_sdk_ffi_iossimulator.a" ]; then rm -rf "${GENERATED_DIR}/libmatrix_sdk_ffi_iossimulator.a"; fi
if [ -d ${HEADERS_DIR} ]; then rm -rf ${HEADERS_DIR}; fi

# Debug -> Copy generated files over to ../../../matrix-rust-components-swift
echo "$(echo "import MatrixSDKFFIWrapper\n"; cat "${SWIFT_DIR}/sdk.swift")" > "${SWIFT_DIR}/sdk.swift"

rsync -a --delete "${GENERATED_DIR}/MatrixSDKFFI.xcframework" "../../../matrix-rust-components-swift/"
rsync -a --delete "${GENERATED_DIR}/swift/" "../../../matrix-rust-components-swift/Sources/MatrixRustSDK"
