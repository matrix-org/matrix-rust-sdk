use std::{
    env,
    error::Error,
    path::{Path, PathBuf},
    process::Command,
};

use vergen_gitcl::{Emitter, GitclBuilder};

/// Adds a temporary workaround for an issue with the Rust compiler and Android
/// in x86_64 devices: https://github.com/rust-lang/rust/issues/109717.
/// The workaround is based on: https://github.com/mozilla/application-services/pull/5442
///
/// IMPORTANT: if you modify this, make sure to modify
/// [../matrix-sdk-crypto-ffi/build.rs] too!
fn setup_x86_64_android_workaround() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("CARGO_CFG_TARGET_OS not set");
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").expect("CARGO_CFG_TARGET_ARCH not set");
    if target_arch == "x86_64" && target_os == "android" {
        // Configure rust to statically link against the `libclang_rt.builtins` supplied
        // with clang.

        // cargo-ndk sets CC_x86_64-linux-android to the path to `clang`, within the
        // Android NDK.
        let clang_path = PathBuf::from(
            env::var("CC_x86_64-linux-android").expect("CC_x86_64-linux-android not set"),
        );

        // clang_path should now look something like
        // `.../sdk/ndk/28.0.12674087/toolchains/llvm/prebuilt/linux-x86_64/bin/clang`.
        // We strip `/bin/clang` from the end to get the toolchain path.
        let toolchain_path = clang_path
            .ancestors()
            .nth(2)
            .expect("could not find NDK toolchain path")
            .to_str()
            .expect("NDK toolchain path is not valid UTF-8");

        let clang_version = get_clang_major_version(&clang_path);

        println!("cargo:rustc-link-search={toolchain_path}/lib/clang/{clang_version}/lib/linux/");
        println!("cargo:rustc-link-lib=static=clang_rt.builtins-x86_64-android");
    }
}

/// Adds a workaround for watchOS simulator builds to manually link against the
/// CoreFoundation framework in order to avoid linker errors. Otherwise, errors
/// like the following may occur:
///
///   = note: Undefined symbols for architecture arm64:
///             "_CFArrayCreate", referenced from:
///             "_CFDataCreate", referenced from:
///             "_CFRelease", referenced from:
///             etc.
fn setup_watchos_simulator_workaround() {
    let target = env::var("TARGET").expect("TARGET not set");
    if target.ends_with("watchos-sim") {
        println!("cargo:rustc-link-arg=-framework");
        println!("cargo:rustc-link-arg=CoreFoundation");
    }
}

/// Run the clang binary at `clang_path`, and return its major version number
fn get_clang_major_version(clang_path: &Path) -> String {
    let clang_output =
        Command::new(clang_path).arg("-dumpversion").output().expect("failed to start clang");

    if !clang_output.status.success() {
        panic!("failed to run clang: {}", String::from_utf8_lossy(&clang_output.stderr));
    }

    let clang_version = String::from_utf8(clang_output.stdout).expect("clang output is not utf8");
    clang_version.split('.').next().expect("could not parse clang output").to_owned()
}

fn main() -> Result<(), Box<dyn Error>> {
    setup_x86_64_android_workaround();
    setup_watchos_simulator_workaround();
    uniffi::generate_scaffolding("./src/api.udl").expect("Building the UDL file failed");

    let git_config = GitclBuilder::default().sha(true).build()?;
    Emitter::default().add_instructions(&git_config)?.emit()?;

    Ok(())
}
