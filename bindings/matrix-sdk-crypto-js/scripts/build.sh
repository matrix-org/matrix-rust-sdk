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
#
# Run this script from the root of the project, i.e. from the parent directory of this file.

set -e

RUSTFLAGS='-C opt-level=z' WASM_BINDGEN_WEAKREF=1 wasm-pack build --release --target nodejs --scope matrix-org --out-dir pkg

# Convert the Wasm into a JS file that exports the base64'ed Wasm.
echo "module.exports = '$(base64 pkg/matrix_sdk_crypto_js_bg.wasm)';" > pkg/matrix_sdk_crypto_js_bg.wasm.js

echo 'HEAD:';
head -c 30 pkg/matrix_sdk_crypto_js_bg.wasm.js

echo -e '\n\nTAIL:';
tail -c 20 pkg/matrix_sdk_crypto_js_bg.wasm.js

# Copy in the unbase64 module
cp scripts/unbase64.js pkg/

# In the JavaScript:
#  1. Replace the lines that load the Wasm,
#  2. Remove the imports of `TextDecoder` and `TextEncoder`. We rely on the global defaults.
loadwasm='const bytes = require("./unbase64.js")(require("./matrix_sdk_crypto_js_bg.wasm.js"));'

if test "$(uname)" = "Darwin"; then
    sed -i '' \
        -e "/^const path = /d" \
        -e "s@^const bytes =.*@${loadwasm}@" \
        -e '/Text..coder.*= require(.util.)/d' \
        pkg/matrix_sdk_crypto_js.js
else
    sed -i \
        -e "/^const path = /d" \
        -e "s@^const bytes =.*@${loadwasm}@" \
        -e '/Text..coder.*= require(.util.)/d' \
        pkg/matrix_sdk_crypto_js.js
fi
