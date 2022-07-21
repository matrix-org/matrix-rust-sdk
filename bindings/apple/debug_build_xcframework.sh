#!/usr/bin/env bash
set -eEu

cd "$(dirname "$0")"

IS_CI=false
ACTIVE_ARCH="arm64"

if [ $# -eq 2 ]; then
  echo "Running CI build"
  IS_CI=true
  ARCHS=( $1 )
  ACTIVE_ARCH=${ARCHS[0]}
elif [ $# -eq 1 ]; then
  echo "Running debug build"
  ARCHS=( $1 )
  ACTIVE_ARCH=${ARCHS[0]}
else
  echo "Running debug build"
fi

echo "Active architecture ${ACTIVE_ARCH}"

# Path to the repo root
SRC_ROOT=../..

TARGET_DIR="${SRC_ROOT}/target"

GENERATED_DIR="${SRC_ROOT}/generated"
mkdir -p ${GENERATED_DIR}

REL_FLAG=""
REL_TYPE_DIR="debug"

# iOS Simulator arm64
if [ "$ACTIVE_ARCH" = "arm64" ]; then
  cargo build -p matrix-sdk-ffi ${REL_FLAG} --target "aarch64-apple-ios-sim"

  lipo -create \
    "${TARGET_DIR}/aarch64-apple-ios-sim/${REL_TYPE_DIR}/libmatrix_sdk_ffi.a" \
    -output "${GENERATED_DIR}/libmatrix_sdk_ffi_iossimulator.a"

# iOS Simulator intel
else 
  cargo build -p matrix-sdk-ffi ${REL_FLAG} --target "x86_64-apple-ios"

  lipo -create \
    "${TARGET_DIR}/x86_64-apple-ios/${REL_TYPE_DIR}/libmatrix_sdk_ffi.a" \
    -output "${GENERATED_DIR}/libmatrix_sdk_ffi_iossimulator.a"
fi

# Generate uniffi files
uniffi-bindgen generate "${SRC_ROOT}/bindings/matrix-sdk-ffi/src/api.udl" --language swift --out-dir ${GENERATED_DIR}

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
  -library "${GENERATED_DIR}/libmatrix_sdk_ffi_iossimulator.a" \
  -headers ${HEADERS_DIR} \
  -output "${GENERATED_DIR}/MatrixSDKFFI.xcframework"

# Cleanup

if [ -f "${GENERATED_DIR}/libmatrix_sdk_ffi_iossimulator.a" ]; then rm -rf "${GENERATED_DIR}/libmatrix_sdk_ffi_iossimulator.a"; fi
if [ -d ${HEADERS_DIR} ]; then rm -rf ${HEADERS_DIR}; fi

if [ "$IS_CI" = false ] ; then
  echo "Preparing matrix-rust-components-swift"

  # Debug -> Copy generated files over to ../../../matrix-rust-components-swift
  rsync -a --delete "${GENERATED_DIR}/MatrixSDKFFI.xcframework" "${SRC_ROOT}/../matrix-rust-components-swift/"
  rsync -a --delete "${GENERATED_DIR}/swift/" "${SRC_ROOT}/../matrix-rust-components-swift/Sources/MatrixRustSDK"
fi
