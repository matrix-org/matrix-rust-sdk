#!/bin/bash

set -ex

yarn install

# Remove generated file to ensure the default `yarn install` on the consumer side
# ends up building its own platform-specific version (if needed).
#
# Forced to ensure repeated builds work (file might not exist)
rm -f lib/index.node

docker build -t rust-sdk-napi -f release/Dockerfile.linux .
docker run --rm -v $(pwd)/../..:/src rust-sdk-napi
