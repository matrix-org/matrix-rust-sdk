// Initially, 'wasm' is set to a Proxy object which will synchronously load the WebAssembly module and
// replace 'wasm' with a reference to the exports from the wasm module.
//
// Ideally this will never get used because the application will call initAsync instead.
wasm = new Proxy(
    {},
    {
        get: (target, prop, receiver) => __initSync()[prop],
    },
);

let inited = false;
const __initSync = function () {
    if (inited) {
        return;
    }
    if (initPromise) {
        throw new Error("Asynchronous initialisation already in progress: cannot initialise synchronously");
    }
    const bytes = unbase64(require("./matrix_sdk_crypto_js_bg.wasm.js"));
    const mod = new WebAssembly.Module(bytes);
    const instance = new WebAssembly.Instance(mod, imports);
    wasm = instance.exports;
    wasm.__wbindgen_start();
    inited = true;
    return wasm;
};

let initPromise = null;

/**
 * Load the WebAssembly module in the background, if it has not already been loaded.
 *
 * Returns a promise which will resolve once the other methods are ready.
 *
 * @returns {Promise<void>}
 */
module.exports.initAsync = function () {
    if (inited) {
        return Promise.resolve();
    }
    if (!initPromise) {
        initPromise = Promise.resolve()
            .then(() => require("./matrix_sdk_crypto_js_bg.wasm.js"))
            .then((b64) => WebAssembly.instantiate(unbase64(b64), imports))
            .then((result) => {
                wasm = result.instance.exports;
                wasm.__wbindgen_start();
                inited = true;
            });
    }
    return initPromise;
};

const b64lookup = new Uint8Array([
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 62, 0, 62, 0, 63, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7,
    8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 0, 0, 0, 0, 63, 0, 26, 27, 28, 29, 30, 31, 32,
    33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51,
]);

// base64 decoder, based on the code at https://developer.mozilla.org/en-US/docs/Glossary/Base64#solution_2_%E2%80%93_rewriting_atob_and_btoa_using_typedarrays_and_utf-8
function unbase64(sBase64) {
    const sB64Enc = sBase64.replace(/[^A-Za-z0-9+/]/g, "");
    const nInLen = sB64Enc.length;
    const nOutLen = (nInLen * 3 + 1) >> 2;
    const taBytes = new Uint8Array(nOutLen);

    let nMod3;
    let nMod4;
    let nUint24 = 0;
    let nOutIdx = 0;
    for (let nInIdx = 0; nInIdx < nInLen; nInIdx++) {
        nMod4 = nInIdx & 3;
        nUint24 |= b64lookup[sB64Enc.charCodeAt(nInIdx)] << (6 * (3 - nMod4));
        if (nMod4 === 3 || nInLen - nInIdx === 1) {
            nMod3 = 0;
            while (nMod3 < 3 && nOutIdx < nOutLen) {
                taBytes[nOutIdx] = (nUint24 >>> ((16 >>> nMod3) & 24)) & 255;
                nMod3++;
                nOutIdx++;
            }
            nUint24 = 0;
        }
    }

    return taBytes;
}
