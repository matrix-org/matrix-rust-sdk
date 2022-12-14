#!/usr/bin/env bash
set -eEu

cd "$(dirname "$0")"
CURRENT_DIR=$(pwd)

# FOR DEBUG
#RELEASE_FLAG=""
#RELEASE_TYPE_DIR="debug"
#RELEASE_AAR_NAME="crypto-android-debug"

# FOR RELEASE
RELEASE_FLAG="--release"
RELEASE_TYPE_DIR="release"
RELEASE_AAR_NAME="crypto-android-release"

SRC_ROOT=../../..
# Path to the kotlin root project
KOTLIN_ROOT=..

BASE_TARGET_DIR="${SRC_ROOT}/target"
SDK_ROOT="${KOTLIN_ROOT}/crypto/crypto-android"
SDK_TARGET_DIR="${SDK_ROOT}/src/main/jniLibs"
BUILD_DIR="${SDK_ROOT}/build"
GENERATED_DIR="${BUILD_DIR}/generated/source/${RELEASE_TYPE_DIR}"
mkdir -p ${GENERATED_DIR}

TARGET_CRATE=matrix-sdk-crypto-ffi

AAR_DESTINATION=$1

# Build libs for all the different architectures

echo -e "Building for x86_64-linux-android[1/4]"
cargo ndk --target x86_64-linux-android -o ${SDK_TARGET_DIR}/ build "${RELEASE_FLAG}" -p ${TARGET_CRATE}

echo -e "Building for aarch64-linux-android[2/4]"
cargo ndk --target aarch64-linux-android -o ${SDK_TARGET_DIR}/ build "${RELEASE_FLAG}" -p ${TARGET_CRATE}

echo -e "Building for armv7-linux-androideabi[3/4]"
cargo ndk --target armv7-linux-androideabi -o ${SDK_TARGET_DIR}/ build "${RELEASE_FLAG}" -p ${TARGET_CRATE}

echo -e "Building for i686-linux-android[4/4]"
cargo ndk --target i686-linux-android -o ${SDK_TARGET_DIR}/ build "${RELEASE_FLAG}" -p ${TARGET_CRATE}

# Generate uniffi files
echo -e "Generate uniffi kotlin file"
uniffi-bindgen generate "${SRC_ROOT}/bindings/${TARGET_CRATE}/src/olm.udl" \
  --language kotlin \
  --config "${SRC_ROOT}/bindings/${TARGET_CRATE}/uniffi.toml" \
  --out-dir ${GENERATED_DIR} \
  --lib-file "${BASE_TARGET_DIR}/x86_64-linux-android/${RELEASE_TYPE_DIR}/libmatrix_sdk_crypto_ffi.a"
  
# Create android library
cd "${KOTLIN_ROOT}"
./gradlew :crypto:crypto-android:assemble
cd "${CURRENT_DIR}"

echo -e "Moving the generated aar file to ${AAR_DESTINATION}/matrix-rust-sdk-crypto.aar"
mv "${BUILD_DIR}/outputs/aar/${RELEASE_AAR_NAME}.aar" "${AAR_DESTINATION}/matrix-rust-sdk-crypto.aar"

# Clean-up
echo -e "Cleaning up temporary files"

rm -r "${BUILD_DIR}"
rm -r "${SDK_TARGET_DIR}"

