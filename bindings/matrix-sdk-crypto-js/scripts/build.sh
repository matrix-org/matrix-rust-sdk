#!/bin/bash
#
# Build the javascript modules
#
# This script is really a workaround for https://github.com/rustwasm/wasm-pack/issues/1074.
#
# Currently, the only reliable way to load webassembly in all the JS
# environments we want to target (web-via-webpack, web-via-browserify, jest)
# seems to be to pack the WASM into base64, and then unpack it and instantiate
# it at runtime.
#
# Hopefully one day, https://github.com/rustwasm/wasm-pack/issues/1074 will be
# fixed and this will be unnecessary.

set -e

RUSTFLAGS='-C opt-level=z' WASM_BINDGEN_WEAKREF=1 wasm-pack build --release --target nodejs --scope matrix-org --out-dir ./pkg

# convert the wasm into a js file that exports the b64'ed wasm
{
    echo 'module.exports = `'
    base64 pkg/matrix_sdk_crypto_js_bg.wasm
    echo '`;'
} > pkg/matrix_sdk_crypto_js_bg.wasm.js

# copy in the unbase64 module
cp unbase64.js pkg/

# In the javascript:
#  1. replace the lines that load the wasm
#  2. remove the imports of TextDecoder and TextEncoder. We rely on the global defaults.
loadwasm='const bytes = require("./unbase64.js")(require("./matrix_sdk_crypto_js_bg.wasm.js"));'
sed -i -e "/^const path = /,+1 c$loadwasm" \
    -e '/= require(`util`)/d' \
    pkg/matrix_sdk_crypto_js.js
