use std::{env, process};

fn env_is_set(var_name: &str) -> bool {
    env::var_os(var_name).is_some()
}

fn ensure(cond: bool, err: &str) {
    if !cond {
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

fn main() {
    // Prevent unnecessary rerunning of this build script
    println!("cargo:rerun-if-changed=build.rs");

    let native_tls_set = env_is_set("CARGO_FEATURE_NATIVE_TLS");
    let rustls_tls_set = env_is_set("CARGO_FEATURE_RUSTLS_TLS");
    ensure(
        native_tls_set || rustls_tls_set,
        "one of the features 'native-tls' or 'rustls-tls' must be enabled",
    );
    ensure(
        !native_tls_set || !rustls_tls_set,
        "only one of the features 'native-tls' or 'rustls-tls' can be enabled",
    );

    let is_wasm = env::var_os("CARGO_CFG_TARGET_ARCH").is_some_and(|arch| arch == "wasm32");
    if is_wasm {
        ensure(
            !env_is_set("CARGO_FEATURE_SSO_LOGIN"),
            "feature 'sso-login' is not available on target arch 'wasm32'",
        );
    }
}
