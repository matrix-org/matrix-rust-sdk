#!/usr/bin/env bash
set -eEu

cd "$(dirname "$0")"
cd ../..

cargo xtask swift build-framework --release $*
