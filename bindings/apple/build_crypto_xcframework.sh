#!/usr/bin/env bash
set -eEu

cd "$(dirname "$0")"

# Path to the repo root
SRC_ROOT=../..

TARGET_DIR="${SRC_ROOT}/target"

GENERATED_DIR="${SRC_ROOT}/generated"
if [ -d "${GENERATED_DIR}" ]; then rm -rf "${GENERATED_DIR}"; fi
mkdir -p ${GENERATED_DIR}

REL_FLAG="--release"
REL_TYPE_DIR="release"

TARGET_CRATE=matrix-sdk-crypto-ffi

# Build static libs for all the different architectures

# iOS
cargo build -p ${TARGET_CRATE} ${REL_FLAG} --target "aarch64-apple-ios"

# iOS Simulator
cargo build -p ${TARGET_CRATE} ${REL_FLAG} --target "aarch64-apple-ios-sim"
cargo build -p ${TARGET_CRATE} ${REL_FLAG} --target "x86_64-apple-ios"

# Lipo together the libraries for the same platform

# iOS Simulator
lipo -create \
  "${TARGET_DIR}/x86_64-apple-ios/${REL_TYPE_DIR}/libmatrix_crypto_ffi.a" \
  "${TARGET_DIR}/aarch64-apple-ios-sim/${REL_TYPE_DIR}/libmatrix_crypto_ffi.a" \
  -output "${GENERATED_DIR}/libmatrix_crypto_ffi.a"

# Generate uniffi files
uniffi-bindgen generate "${SRC_ROOT}/crates/${TARGET_CRATE}/src/olm.udl" --language swift --config-path "${SRC_ROOT}/crates/${TARGET_CRATE}/uniffi.toml" --out-dir ${GENERATED_DIR}

# Move headers to the right place
HEADERS_DIR=${GENERATED_DIR}/headers
mkdir -p ${HEADERS_DIR}
mv ${GENERATED_DIR}/*.h ${HEADERS_DIR}

# Rename and move modulemap to the right place
mv ${GENERATED_DIR}/*.modulemap ${HEADERS_DIR}/module.modulemap

# Move source files to the right place
SWIFT_DIR="${GENERATED_DIR}/Sources"
mkdir -p ${SWIFT_DIR}
mv ${GENERATED_DIR}/*.swift ${SWIFT_DIR}

# Build the xcframework

if [ -d "${GENERATED_DIR}/MatrixSDKCryptoFFI.xcframework" ]; then rm -rf "${GENERATED_DIR}/MatrixSDKCryptoFFI.xcframework"; fi

xcodebuild -create-xcframework \
  -library "${TARGET_DIR}/aarch64-apple-ios/${REL_TYPE_DIR}/libmatrix_crypto_ffi.a" \
  -headers ${HEADERS_DIR} \
  -library "${GENERATED_DIR}/libmatrix_crypto_ffi.a" \
  -headers ${HEADERS_DIR} \
  -output "${GENERATED_DIR}/MatrixSDKCryptoFFI.xcframework"

# Cleanup

if [ -f "${TARGET_DIR}/aarch64-apple-ios-sim/${REL_TYPE_DIR}/libmatrix_crypto_ffi.a" ]; then rm -rf "${TARGET_DIR}/aarch64-apple-ios-sim/${REL_TYPE_DIR}/libmatrix_crypto_ffi.a"; fi
if [ -f "${GENERATED_DIR}/libmatrix_crypto_ffi.a" ]; then rm -rf "${GENERATED_DIR}/libmatrix_crypto_ffi.a"; fi
if [ -d ${HEADERS_DIR} ]; then rm -rf ${HEADERS_DIR}; fi

# Zip up framework, sources and LICENSE, ready to be uploaded to GitHub Releases and used by MatrixSDKCrypto.podspec
cp ${SRC_ROOT}/LICENSE $GENERATED_DIR
cd $GENERATED_DIR
zip -r MatrixSDKCryptoFFI.zip MatrixSDKCryptoFFI.xcframework Sources LICENSE
