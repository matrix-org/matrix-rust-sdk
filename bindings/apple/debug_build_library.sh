#!/usr/bin/env bash
set -eEu

cd "$(dirname "$0")"

echo "Running debug library build"

# Path to the repo root
SRC_ROOT=../..

TARGET_DIR="${SRC_ROOT}/target"
FFI_DIR="${SRC_ROOT}/bindings/apple/generated/matrix_sdk_ffi"
SWIFT_DIR="${SRC_ROOT}/bindings/apple/generated/swift"
REL_TYPE_DIR="debug"

mkdir -p ${FFI_DIR}
mkdir -p ${SWIFT_DIR}

cargo build -p matrix-sdk-ffi

# Move compiled library
mv "${TARGET_DIR}/${REL_TYPE_DIR}/libmatrix_sdk_ffi.a" "${FFI_DIR}"

# Generate uniffi files
uniffi-bindgen generate \
  --language swift \
  --lib-file "${FFI_DIR}/libmatrix_sdk_ffi.a" \
  --out-dir ${FFI_DIR} \
  "${SRC_ROOT}/bindings/matrix-sdk-ffi/src/api.udl"

# Rename modulemap
if [ -f "${FFI_DIR}/module.modulemap" ]; then rm -rf "${FFI_DIR}/module.modulemap"; fi
mv ${FFI_DIR}/*.modulemap ${FFI_DIR}/module.modulemap

# Move swift sources
mv ${FFI_DIR}/*.swift ${SWIFT_DIR}

