#!/usr/bin/env bash
set -eEu

cd "$(dirname "$0")"

# Path to the repo root
SRC_ROOT=../..

TARGET_DIR="${SRC_ROOT}/target"

GENERATED_DIR="${SRC_ROOT}/generated"
mkdir -p ${GENERATED_DIR}

REL_FLAG="--release"
REL_TYPE_DIR="release"

# Build static libs for all the different architectures

# iOS
echo -e "Building for iOS [1/5]"
cargo build -p matrix-sdk-ffi ${REL_FLAG} --target "aarch64-apple-ios"

# MacOS
echo -e "\nBuilding for macOS (Apple Silicon) [2/5]"
cargo build -p matrix-sdk-ffi ${REL_FLAG} --target "aarch64-apple-darwin"
echo -e "\nBuilding for macOS (Intel) [3/5]"
cargo build -p matrix-sdk-ffi ${REL_FLAG} --target "x86_64-apple-darwin"

# iOS Simulator
echo -e "\nBuilding for iOS Simulator (Apple Silicon) [4/5]"
cargo build -p matrix-sdk-ffi ${REL_FLAG} --target "aarch64-apple-ios-sim"
echo -e "\nBuilding for iOS Simulator (Intel) [5/5]"
cargo build -p matrix-sdk-ffi ${REL_FLAG} --target "x86_64-apple-ios"

echo -e "\nCreating XCFramework"
# Lipo together the libraries for the same platform

# MacOS
lipo -create \
  "${TARGET_DIR}/x86_64-apple-darwin/${REL_TYPE_DIR}/libmatrix_sdk_ffi.a" \
  "${TARGET_DIR}/aarch64-apple-darwin/${REL_TYPE_DIR}/libmatrix_sdk_ffi.a" \
  -output "${GENERATED_DIR}/libmatrix_sdk_ffi_macos.a"

# iOS Simulator
lipo -create \
  "${TARGET_DIR}/x86_64-apple-ios/${REL_TYPE_DIR}/libmatrix_sdk_ffi.a" \
  "${TARGET_DIR}/aarch64-apple-ios-sim/${REL_TYPE_DIR}/libmatrix_sdk_ffi.a" \
  -output "${GENERATED_DIR}/libmatrix_sdk_ffi_iossimulator.a"


# Generate uniffi files
# Architecture for the .a file argument doesn't matter, since the API is the same on all
uniffi-bindgen generate \
  --language swift \
  --cdylib "${TARGET_DIR}/x86_64-apple-darwin/${REL_TYPE_DIR}/libmatrix_sdk_ffi.a" \
  --out-dir ${GENERATED_DIR} \
  "${SRC_ROOT}/bindings/matrix-sdk-ffi/src/api.udl"

# Move them to the right place
HEADERS_DIR=${GENERATED_DIR}/headers
mkdir -p ${HEADERS_DIR}

mv ${GENERATED_DIR}/*.h ${HEADERS_DIR}

# Rename and move modulemap to the right place
mv ${GENERATED_DIR}/*.modulemap ${HEADERS_DIR}/module.modulemap

SWIFT_DIR="${GENERATED_DIR}/swift"
mkdir -p ${SWIFT_DIR}

mv ${GENERATED_DIR}/*.swift ${SWIFT_DIR}

# Build the xcframework

if [ -d "${GENERATED_DIR}/MatrixSDKFFI.xcframework" ]; then rm -rf "${GENERATED_DIR}/MatrixSDKFFI.xcframework"; fi

xcodebuild -create-xcframework \
  -library "${GENERATED_DIR}/libmatrix_sdk_ffi_macos.a" \
  -headers ${HEADERS_DIR} \
  -library "${GENERATED_DIR}/libmatrix_sdk_ffi_iossimulator.a" \
  -headers ${HEADERS_DIR} \
  -library "${TARGET_DIR}/aarch64-apple-ios/${REL_TYPE_DIR}/libmatrix_sdk_ffi.a" \
  -headers ${HEADERS_DIR} \
  -output "${GENERATED_DIR}/MatrixSDKFFI.xcframework"

# Cleanup

if [ -f "${GENERATED_DIR}/libmatrix_sdk_ffi_macos.a" ]; then rm -rf "${GENERATED_DIR}/libmatrix_sdk_ffi_macos.a"; fi
if [ -f "${GENERATED_DIR}/libmatrix_sdk_ffi_iossimulator.a" ]; then rm -rf "${GENERATED_DIR}/libmatrix_sdk_ffi_iossimulator.a"; fi
if [ -d ${HEADERS_DIR} ]; then rm -rf ${HEADERS_DIR}; fi
