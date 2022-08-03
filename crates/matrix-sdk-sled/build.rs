use std::{env, process};

fn main() {
    let target_arch = env::var_os("CARGO_CFG_TARGET_ARCH");
    if target_arch.map_or(false, |arch| arch == "wasm32") {
        let err = "this crate does not support the target arch 'wasm32'";
        eprintln!(
            "\n\
            ┏━━━━━━━━{pad}━┓\n\
            ┃ error: {err} ┃\n\
            ┗━━━━━━━━{pad}━┛\n\
            ",
            pad = "━".repeat(err.len()),
        );
        process::exit(1);
    }
}
