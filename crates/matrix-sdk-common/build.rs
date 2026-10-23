use std::{env, process};

fn main() {
    // Prevent unnecessary rerunning of this build script
    println!("cargo:rerun-if-changed=build.rs");

    let is_wasm = env::var_os("CARGO_CFG_TARGET_ARCH").is_some_and(|arch| arch == "wasm32");
    let is_wasi = env::var_os("CARGO_CFG_TARGET_OS").is_some_and(|os| os == "wasi");

    if is_wasm && !is_wasi && env::var_os("CARGO_FEATURE_JS").is_none() {        
        eprintln!(
            "\n\
            ┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓\n\
            ┃ error: matrix-sdk currently requires a JavaScript environment for WASM. ┃\n\
            ┃        Please activate the `js` Cargo feature if this is what you want. ┃\n\
            ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛\n\
            ",
        );
        process::exit(1);
    }
}
