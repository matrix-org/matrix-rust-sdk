## Example usage of matrix-rust-sdk from WASM

This example is a version of the
[command bot](https://github.com/matrix-org/matrix-rust-sdk/tree/main/examples/command_bot)
that runs as WASM in your browser instead of natively on your machine.

To run this example, first ensure:

1. The wasm-unknown-unknown target is available:
   `rustup target add wasm32-unknown-unknown`.
2. `wasm-pack` is in the path:
   [see instructions here](https://rustwasm.github.io/wasm-pack/installer/).
3. A homeserver is available at `http://localhost:8008` with a user `username`
   and password `wordpass`. If this is not the case, you'll want to update these
   connection settings in `src/lib.rs` before building.

You can then build the example locally with:

    npm install
    npm run serve

and then visiting http://localhost:8080 in a browser should run the example!

This example is loosely based off of
[this example](https://github.com/seanmonstar/reqwest/tree/master/examples/wasm_github_fetch),
an example usage of `fetch` from `wasm-bindgen`.

NOTE: The webpage at `http://localhost:8080` will not display anything. It just
runs the bot on the page, printing log messages to the console and responding to
the `!party` command.
