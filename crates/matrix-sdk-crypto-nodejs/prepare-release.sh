#!/bin/bash

set -ex

yarn install

# Remove generated file to ensure the default `yarn install` on the consumer side
# ends up building its own platform-specific version (if needed).
rm lib/index.node

docker build -t rust-sdk-napi -f release/Dockerfile.linux .
docker run --rm -v $(pwd)/../..:/src rust-sdk-napi
