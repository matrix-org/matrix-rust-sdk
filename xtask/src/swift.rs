use clap::{Args, Subcommand};
use std::fs::{self, create_dir_all};
use std::path::PathBuf;
use xshell::{cmd, pushd};

use crate::{workspace, Result};

/// Builds the SDK for Swift as a Static Library or XCFramework.
#[derive(Args)]
pub struct SwiftArgs {
    #[clap(subcommand)]
    cmd: SwiftCommand,
}

#[derive(Subcommand)]
enum SwiftCommand {
    /// Builds the SDK for Swift as a static lib.
    BuildLibrary,
    /// Builds the SDK for Swift as an XCFramework.
    BuildFramework {
        /// Build in release mode
        #[clap(long)]
        release: bool,
        /// Where to rsync the resulting framework to
        #[clap(long)]
        rsync_path: Option<PathBuf>,
    },
}

impl SwiftArgs {
    pub fn run(self) -> Result<()> {
        let _p = pushd(workspace::root_path()?)?;

        match self.cmd {
            SwiftCommand::BuildLibrary => build_library(),
            SwiftCommand::BuildFramework { release, rsync_path } => {
                build_xcframework(release, rsync_path)
            }
        }
    }
}

fn build_library() -> Result<()> {
    println!("Running debug library build.");

    let release_type = "debug";
    let static_lib_filename = "libmatrix_sdk_ffi.a";

    let root_directory = workspace::root_path()?;
    let target_directory = root_directory.join("target");
    let ffi_directory = root_directory.join("bindings/apple/generated/matrix_sdk_ffi");
    let library_file = ffi_directory.join(static_lib_filename);

    create_dir_all(ffi_directory.as_path())?;

    cmd!("cargo build -p matrix-sdk-ffi").run()?;

    fs::rename(
        target_directory.join(release_type).join(static_lib_filename),
        ffi_directory.join(static_lib_filename),
    )?;
    generate_uniffi(&library_file, &ffi_directory)?;
    Ok(())
}

fn generate_uniffi(library_file: &PathBuf, ffi_directory: &PathBuf) -> Result<()> {
    let root_directory = workspace::root_path()?;
    let udl_file = camino::Utf8PathBuf::from_path_buf(
        root_directory.join("bindings/matrix-sdk-ffi/src/api.udl"),
    )
    .unwrap();
    let outdir_overwrite = camino::Utf8PathBuf::from_path_buf(ffi_directory.clone()).unwrap();
    let swift_directory = root_directory.join("bindings/apple/generated/swift");
    let library_path = camino::Utf8PathBuf::from_path_buf(library_file.clone()).unwrap();

    create_dir_all(swift_directory.as_path())?;

    uniffi_bindgen::generate_bindings(
        udl_file.as_path(),
        None,
        vec!["swift"],
        Some(outdir_overwrite.as_path()),
        Some(library_path.as_path()),
        false,
    )?;

    let module_map_file = ffi_directory.join("module.modulemap");
    if module_map_file.exists() {
        fs::remove_file(module_map_file.as_path())?;
    }

    // TODO: Find the modulemap in the ffi directory.
    fs::rename(ffi_directory.join("matrix_sdk_ffiFFI.modulemap"), module_map_file)?;
    // TODO: Move all swift files.
    fs::rename(
        ffi_directory.join("matrix_sdk_ffi.swift"),
        swift_directory.join("matrix_sdk_ffi.swift"),
    )?;

    Ok(())
}

fn build_for_target(target: &str, release: bool) -> Result<PathBuf> {
    let (release_flag, rel_type_dir) =
        if release { ("--release", "release") } else { ("", "debug") };
    cmd!("cargo build -p matrix-sdk-ffi {release_flag} --target {target}").run()?;
    Ok(workspace::root_path()?
        .join("target")
        .join(target)
        .join(rel_type_dir)
        .join("libmatrix_sdk_ffi.a"))
}

fn build_xcframework(release_mode: bool, rsync_path: Option<PathBuf>) -> Result<()> {
    println!("-- Building for iOS [1/5]");
    let ios_path = build_for_target("aarch64-apple-ios", release_mode)?;

    println!("-- Building for macOS (Apple Silicon) [2/5]");
    let darwin_arm_path = build_for_target("aarch64-apple-darwin", release_mode)?;
    println!("-- Building for macOS (Intel) [3/5]");
    let darwin_x86_path = build_for_target("x86_64-apple-darwin", release_mode)?;

    println!("-- Building for iOS Simulator (Apple Silicon) [4/5]");
    let ios_sim_arm_path = build_for_target("aarch64-apple-ios-sim", release_mode)?;
    println!("-- Building for iOS Simulator (Intel) [5/5]");
    let ios_sim_x86_path = build_for_target("x86_64-apple-ios", release_mode)?;

    println!("-- Creating XCFramework");

    let root_dir = workspace::root_path()?;
    let generated_dir = root_dir.join("generated");
    let headers_dir = generated_dir.join("headers");
    create_dir_all(headers_dir.clone())?;

    // # MacOS
    let lipo_target_macos = generated_dir.join("libmatrix_sdk_ffi_macos.a");
    println!("-- Running Lipo for macOS [1/4]");
    cmd!(
        "lipo -create {darwin_x86_path} {darwin_arm_path}
        -output {lipo_target_macos}"
    )
    .run()?;

    // # iOS Simulator
    let lipo_target_sim = generated_dir.join("libmatrix_sdk_ffi_iossimulator.a");
    println!("-- Running Lipo for iOS [2/4]");
    cmd!(
        "lipo -create {ios_sim_arm_path} {ios_sim_x86_path}
        -output {lipo_target_sim}"
    )
    .run()?;

    println!("-- Generate uniffi files [3/4]");
    generate_uniffi(&darwin_x86_path, &generated_dir)?;

    fs::rename(generated_dir.join("matrix_sdk_ffiFFI.h"), headers_dir.join("matrix_sdk_ffiFFI.h"))?;

    println!("-- Generate MatrixSDKFFI.xcframework framework [4/4]");
    let xcframework_path = generated_dir.join("MatrixSDKFFI.xcframework");
    if xcframework_path.exists() {
        fs::remove_dir_all(xcframework_path.as_path())?;
    }

    cmd!(
        "xcodebuild -create-xcframework
        -library {lipo_target_macos}
        -headers {headers_dir}
        -library {lipo_target_sim}
        -headers {headers_dir}
        -library {ios_path}
        -headers {headers_dir}
        -output {generated_dir}/MatrixSDKFFI.xcframework
    "
    )
    .run()?;

    // cleaning up the intermediate data
    fs::remove_file(lipo_target_macos.as_path())?;
    fs::remove_file(lipo_target_sim.as_path())?;
    fs::remove_dir_all(headers_dir.as_path())?;

    if let Some(path) = rsync_path {
        println!("-- Rsync MatrixSDKFFI.xcframework to {path:?}");
        cmd!("rsync -a --delete {generated_dir}/MatrixSDKFFI.xcframework {path}").run()?;
        cmd!("rsync -a --delete {generated_dir}/swift/ {path}/Sources/MatrixRustSDK").run()?;
    }

    println!("-- All done and honkey dorey. Enjoy!");

    Ok(())
}
