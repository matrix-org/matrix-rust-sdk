use std::{env, process};

fn main() {
    let is_wasm = env::var_os("CARGO_CFG_TARGET_ARCH").is_some_and(|arch| arch == "wasm32");
    if is_wasm && env::var_os("CARGO_FEATURE_JS").is_none() {
        eprintln!(
            "\n\
            ┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓\n\
            ┃ error: matrix-sdk currently requires a JavaScript environment for WASM. ┃\n\
            ┃        Please activate the `js` Cargo feature if this is what you want. ┃\n\
            ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛\n\
            ",
        );
        process::exit(1);
    };
}
