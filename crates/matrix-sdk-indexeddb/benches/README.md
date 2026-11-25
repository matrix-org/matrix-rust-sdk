# Benches

This directory contains benchmarks which are compatible with [`wasm-bindgen`][wbg] and can be used to benchmark
things like IndexedDB. This is possible through [`wasm-bindgen-test`][wbg-t]s modified version of [`criterion`][criterion].
While this makes it possible to benchmark browser API's, it is also rather limited and has only the most basic features
of `criterion`.

For more details on how to use `wasm-bindgen-test`, see the testing and benchmarking [guide][wbg-t-guide].

## Running the Benchmarks

One can run the benchmarks by executing the following command within the `matrix-sdk-indexeddb` crate.

```
WASM_BINDGEN_TEST_TIMEOUT=1200 cargo bench --package matrix-sdk-indexeddb --target=wasm32-unknown-unknown
```

By default, this will try running the benchmarks in a headless Chrome browser. This can be changed using
environment variables as described in [here][wbg-t-guide-browser].

Note that the timeout has been overridden, as the benchmarks take longer than the default timeout.

## Persisting the Benchmarks

Unfortunately, there is no support for options like `--save-baseline` in `wasm-bindgen-test`; however,
benchmarks can be written to different files using the following environment variable: `WASM_BINDGEN_BENCH_RESULT`.

[wbg]: https://docs.rs/wasm-bindgen/latest/wasm_bindgen/
[wbg-t]: https://docs.rs/wasm-bindgen-test/latest/wasm_bindgen_test/
[wbg-t-guide]: https://wasm-bindgen.github.io/wasm-bindgen/wasm-bindgen-test/index.html
[wbg-t-guide-browser]: https://wasm-bindgen.github.io/wasm-bindgen/wasm-bindgen-test/index.html
[criterion]: https://docs.rs/wasm-bindgen-test/latest/wasm_bindgen_test/__rt/criterion/index.html
