#!/usr/bin/env bash
set -eEu

helpFunction() {
  echo ""
  echo "Usage: $0 -only_ios"
  echo -e "\t-i Option to build only for iOS. Default will build for all targets."
  exit 1
}

only_ios='false'

while getopts ':i' 'opt'; do
  case ${opt} in
  'i') only_ios='true' ;;
  ?) helpFunction ;;
  esac
done

cd "$(dirname "$0")"

# Path to the repo root
SRC_ROOT=../..

TARGET_DIR="${SRC_ROOT}/target"

GENERATED_DIR="${SRC_ROOT}/generated"
if [ -d "${GENERATED_DIR}" ]; then rm -rf "${GENERATED_DIR}"; fi
mkdir -p ${GENERATED_DIR}/{macos,simulator}

REL_FLAG="--release"
REL_TYPE_DIR="release"

TARGET_CRATE=matrix-sdk-crypto-ffi

# Build static libs for all the different architectures

# Required by olm-sys crate
export IOS_SDK_PATH=`xcrun --show-sdk-path --sdk iphoneos`

if ${only_ios}; then
  # iOS
  echo -e "Building only for iOS"
  cargo build -p ${TARGET_CRATE} ${REL_FLAG} --target "aarch64-apple-ios"
else
  # iOS
  echo -e "Building for iOS [1/5]"
  cargo build -p ${TARGET_CRATE} ${REL_FLAG} --target "aarch64-apple-ios"

  # MacOS
  echo -e "\nBuilding for macOS (Apple Silicon) [2/5]"
  cargo build -p ${TARGET_CRATE} ${REL_FLAG} --target "aarch64-apple-darwin"
  echo -e "\nBuilding for macOS (Intel) [3/5]"
  cargo build -p ${TARGET_CRATE} ${REL_FLAG} --target "x86_64-apple-darwin"

  # iOS Simulator
  echo -e "\nBuilding for iOS Simulator (Apple Silicon) [4/5]"
  cargo build -p ${TARGET_CRATE} ${REL_FLAG} --target "aarch64-apple-ios-sim"
  echo -e "\nBuilding for iOS Simulator (Intel) [5/5]"
  cargo build -p ${TARGET_CRATE} ${REL_FLAG} --target "x86_64-apple-ios"
fi

echo -e "\nCreating XCFramework"
# Lipo together the libraries for the same platform

if ! ${only_ios}; then
  echo "Lipo together the libraries for the same platform"
  # MacOS
  lipo -create \
    "${TARGET_DIR}/x86_64-apple-darwin/${REL_TYPE_DIR}/libmatrix_sdk_crypto_ffi.a" \
    "${TARGET_DIR}/aarch64-apple-darwin/${REL_TYPE_DIR}/libmatrix_sdk_crypto_ffi.a" \
    -output "${GENERATED_DIR}/macos/libmatrix_sdk_crypto_ffi.a"

  # iOS Simulator
  lipo -create \
    "${TARGET_DIR}/x86_64-apple-ios/${REL_TYPE_DIR}/libmatrix_sdk_crypto_ffi.a" \
    "${TARGET_DIR}/aarch64-apple-ios-sim/${REL_TYPE_DIR}/libmatrix_sdk_crypto_ffi.a" \
    -output "${GENERATED_DIR}/simulator/libmatrix_sdk_crypto_ffi.a"
fi

# Generate uniffi files
cd ../matrix-sdk-crypto-ffi && cargo run --bin matrix_sdk_crypto_ffi generate \
  --language swift \
  --library "${TARGET_DIR}/aarch64-apple-ios/${REL_TYPE_DIR}/libmatrix_sdk_crypto_ffi.a" \
  --out-dir ${GENERATED_DIR}

# Move headers to the right place
HEADERS_DIR=${GENERATED_DIR}/headers
mkdir -p ${HEADERS_DIR}
mv ${GENERATED_DIR}/*.h ${HEADERS_DIR}

# Rename and merge the modulemap files into a single file to the right place
for f in ${GENERATED_DIR}/*.modulemap
do 
  cat $f; echo; 
done > ${HEADERS_DIR}/module.modulemap
rm ${GENERATED_DIR}/*.modulemap

# Move source files to the right place
SWIFT_DIR="${GENERATED_DIR}/Sources"
mkdir -p ${SWIFT_DIR}
mv ${GENERATED_DIR}/*.swift ${SWIFT_DIR}

# Build the xcframework

if [ -d "${GENERATED_DIR}/MatrixSDKCryptoFFI.xcframework" ]; then rm -rf "${GENERATED_DIR}/MatrixSDKCryptoFFI.xcframework"; fi
if ${only_ios}; then
  xcodebuild -create-xcframework \
    -library "${TARGET_DIR}/aarch64-apple-ios/${REL_TYPE_DIR}/libmatrix_sdk_crypto_ffi.a" \
    -headers ${HEADERS_DIR} \
    -output "${GENERATED_DIR}/MatrixSDKCryptoFFI.xcframework"
else
  xcodebuild -create-xcframework \
    -library "${TARGET_DIR}/aarch64-apple-ios/${REL_TYPE_DIR}/libmatrix_sdk_crypto_ffi.a" \
    -headers ${HEADERS_DIR} \
    -library "${GENERATED_DIR}/macos/libmatrix_sdk_crypto_ffi.a" \
    -headers ${HEADERS_DIR} \
    -library "${GENERATED_DIR}/simulator/libmatrix_sdk_crypto_ffi.a" \
    -headers ${HEADERS_DIR} \
    -output "${GENERATED_DIR}/MatrixSDKCryptoFFI.xcframework"
fi

# Cleanup
if [ -d "${GENERATED_DIR}/macos" ]; then rm -rf "${GENERATED_DIR}/macos"; fi
if [ -d "${GENERATED_DIR}/simulator" ]; then rm -rf "${GENERATED_DIR}/simulator"; fi
if [ -d ${HEADERS_DIR} ]; then rm -rf ${HEADERS_DIR}; fi

# Zip up framework, sources and LICENSE, ready to be uploaded to GitHub Releases and used by MatrixSDKCrypto.podspec
cp ${SRC_ROOT}/LICENSE $GENERATED_DIR
cd $GENERATED_DIR
zip -r MatrixSDKCryptoFFI.zip MatrixSDKCryptoFFI.xcframework Sources LICENSE
rm LICENSE

echo "XCFramework is ready 🚀"
