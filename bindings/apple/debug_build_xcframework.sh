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

# iOS Simulator arm64
if [ "$ACTIVE_ARCH" = "arm64" ]; then
  TARGET="aarch64-apple-ios-sim"
# iOS Simulator intel
else
  TARGET="x86_64-apple-ios"
fi
echo "Active architecture ${ACTIVE_ARCH}"

cargo xtask swift build-framework --profile=reldbg --sim-only-target=${TARGET} $*
