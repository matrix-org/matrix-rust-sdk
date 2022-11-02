#!/bin/bash
#
# Build the JavaScript modules
#
# This script is really a workaround for https://github.com/rustwasm/wasm-pack/issues/1074.
#
# Currently, the only reliable way to load WebAssembly in all the JS
# environments we want to target (web-via-webpack, web-via-browserify, jest)
# seems to be to pack the WASM into base64, and then unpack it and instantiate
# it at runtime.
#
# Hopefully one day, https://github.com/rustwasm/wasm-pack/issues/1074 will be
# fixed and this will be unnecessary.

set -e

cd $(dirname "$0")/..

RUSTFLAGS='-C opt-level=z' WASM_BINDGEN_WEAKREF=1 wasm-pack build --release --target nodejs --scope matrix-org --out-dir pkg

# Convert the Wasm into a JS file that exports the base64'ed Wasm.
echo "module.exports = \`$(base64 pkg/matrix_sdk_crypto_js_bg.wasm)\`;" > pkg/matrix_sdk_crypto_js_bg.wasm.js

# Copy in the unbase64 module
cp scripts/unbase64.js pkg/

# In the JavaScript:
#  1. Replace the lines that load the Wasm,
#  2. Remove the imports of `TextDecoder` and `TextEncoder`. We rely on the global defaults.
loadwasm='const bytes = require("./unbase64.js")(require("./matrix_sdk_crypto_js_bg.wasm.js"));'

# sed on OSX uses different syntax for sed -i, so let's just avoid it.
sed -e "/^const path = /d" \
    -e "s@^const bytes =.*@${loadwasm}@" \
    -e '/Text..coder.*= require(.util.)/d' \
    pkg/matrix_sdk_crypto_js.js >pkg/matrix_sdk_crypto_js.js.new
mv pkg/matrix_sdk_crypto_js.js.new pkg/matrix_sdk_crypto_js.js
