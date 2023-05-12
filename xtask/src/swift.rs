use std::{
    fs::{copy, create_dir_all, remove_dir_all, remove_file, rename},
    path::{Path, PathBuf},
};

use clap::{Args, Subcommand};
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
        /// Build with the release profile
        #[clap(long)]
        release: bool,

        /// Build with a custom profile, takes precedence over `--release`
        #[clap(long)]
        profile: Option<String>,

        /// Build the given target only
        #[clap(long)]
        only_target: Option<String>,

        /// Move the generated xcframework and swift sources into the given
        /// components-folder
        #[clap(long)]
        components_path: Option<PathBuf>,
    },
}

impl SwiftArgs {
    pub fn run(self) -> Result<()> {
        let _p = pushd(workspace::root_path()?)?;

        match self.cmd {
            SwiftCommand::BuildLibrary => build_library(),
            SwiftCommand::BuildFramework { release, profile, components_path, only_target } => {
                let profile = profile.as_deref().unwrap_or(if release { "release" } else { "dev" });
                build_xcframework(profile, only_target, components_path)
            }
        }
    }
}

fn build_library() -> Result<()> {
    println!("Running debug library build.");

    let release_type = "debug";
    let static_lib_filename = "libmatrix_sdk_ffi.a";

    let root_directory = workspace::root_path()?;
    let target_directory = workspace::target_path()?;
    let ffi_directory = root_directory.join("bindings/apple/generated/matrix_sdk_ffi");
    let library_file = ffi_directory.join(static_lib_filename);

    create_dir_all(ffi_directory.as_path())?;

    cmd!("cargo build -p matrix-sdk-ffi").run()?;

    rename(
        target_directory.join(release_type).join(static_lib_filename),
        ffi_directory.join(static_lib_filename),
    )?;
    let swift_directory = root_directory.join("bindings/apple/generated/swift");
    create_dir_all(swift_directory.as_path())?;

    generate_uniffi(&library_file, &ffi_directory)?;

    let module_map_file = ffi_directory.join("module.modulemap");
    if module_map_file.exists() {
        remove_file(module_map_file.as_path())?;
    }

    // TODO: Find the modulemap in the ffi directory.
    rename(ffi_directory.join("matrix_sdk_ffiFFI.modulemap"), module_map_file)?;
    // TODO: Move all swift files.
    rename(
        ffi_directory.join("matrix_sdk_ffi.swift"),
        swift_directory.join("matrix_sdk_ffi.swift"),
    )?;
    Ok(())
}

fn generate_uniffi(library_file: &Path, ffi_directory: &Path) -> Result<()> {
    let root_directory = workspace::root_path()?;
    let udl_file = camino::Utf8PathBuf::from_path_buf(
        root_directory.join("bindings/matrix-sdk-ffi/src/api.udl"),
    )
    .unwrap();
    let outdir_overwrite = camino::Utf8Path::from_path(ffi_directory).unwrap();
    let library_path = camino::Utf8Path::from_path(library_file).unwrap();

    uniffi_bindgen::generate_bindings(
        udl_file.as_path(),
        None,
        vec!["swift"],
        Some(outdir_overwrite),
        Some(library_path),
        false,
    )?;
    Ok(())
}

fn build_for_target(target: &str, profile: &str) -> Result<PathBuf> {
    cmd!("cargo build -p matrix-sdk-ffi --target {target} --profile {profile}").run()?;

    // The builtin dev profile has its files stored under target/debug, all
    // other targets have matching directory names
    let profile_dir_name = if profile == "dev" { "debug" } else { profile };
    Ok(workspace::target_path()?.join(target).join(profile_dir_name).join("libmatrix_sdk_ffi.a"))
}

fn build_xcframework(
    profile: &str,
    only_target: Option<String>,
    components_path: Option<PathBuf>,
) -> Result<()> {
    let root_dir = workspace::root_path()?;
    let apple_dir = root_dir.join("bindings/apple");
    let generated_dir = apple_dir.join("generated");

    // Cleanup destination folder
    let _ = remove_dir_all(&generated_dir);
    create_dir_all(&generated_dir)?;

    let headers_dir = generated_dir.join("ls");
    let swift_dir = generated_dir.join("swift");
    create_dir_all(headers_dir.clone())?;
    create_dir_all(swift_dir.clone())?;

    let (libs, uniff_lib_path) = if let Some(target) = only_target {
        println!("-- Building for {target} 1/1");
        let build_path = build_for_target(target.as_str(), profile)?;

        (vec![build_path.clone()], build_path)
    } else {
        println!("-- Building for iOS [1/5]");
        let ios_path = build_for_target("aarch64-apple-ios", profile)?;

        println!("-- Building for macOS (Apple Silicon) [2/5]");
        let darwin_arm_path = build_for_target("aarch64-apple-darwin", profile)?;
        println!("-- Building for macOS (Intel) [3/5]");
        let darwin_x86_path = build_for_target("x86_64-apple-darwin", profile)?;

        println!("-- Building for iOS Simulator (Apple Silicon) [4/5]");
        let ios_sim_arm_path = build_for_target("aarch64-apple-ios-sim", profile)?;
        println!("-- Building for iOS Simulator (Intel) [5/5]");
        let ios_sim_x86_path = build_for_target("x86_64-apple-ios", profile)?;

        println!("-- Running Lipo for macOS [1/2]");
        // # macOS
        let lipo_target_macos = generated_dir.join("libmatrix_sdk_ffi_macos.a");
        cmd!(
            "lipo -create {darwin_x86_path} {darwin_arm_path}
            -output {lipo_target_macos}"
        )
        .run()?;

        println!("-- Running Lipo for iOS Simulator [2/2]");
        // # iOS Simulator
        let lipo_target_sim = generated_dir.join("libmatrix_sdk_ffi_iossimulator.a");
        cmd!(
            "lipo -create {ios_sim_arm_path} {ios_sim_x86_path}
            -output {lipo_target_sim}"
        )
        .run()?;
        (vec![lipo_target_macos, lipo_target_sim, ios_path], darwin_x86_path)
    };

    println!("-- Generating uniffi files");
    generate_uniffi(&uniff_lib_path, &generated_dir)?;

    rename(generated_dir.join("matrix_sdk_ffiFFI.h"), headers_dir.join("matrix_sdk_ffiFFI.h"))?;

    // Move and rename the module map to `module.modulemap` to match what
    // the xcframework expects
    rename(
        generated_dir.join("matrix_sdk_ffiFFI.modulemap"),
        headers_dir.join("module.modulemap"),
    )?;

    rename(generated_dir.join("matrix_sdk_ffi.swift"), swift_dir.join("matrix_sdk_ffi.swift"))?;

    println!("-- Generating MatrixSDKFFI.xcframework framework");
    let xcframework_path = generated_dir.join("MatrixSDKFFI.xcframework");
    if xcframework_path.exists() {
        remove_dir_all(xcframework_path.as_path())?;
    }
    let mut cmd = cmd!("xcodebuild -create-xcframework");
    for p in libs {
        cmd = cmd.arg("-library").arg(p).arg("-headers").arg(headers_dir.as_path())
    }
    cmd.arg("-output").arg(xcframework_path.as_path()).run()?;

    // Copy the Swift package manifest to the SDK root for local development.
    copy(apple_dir.join("Debug-Package.swift"), root_dir.join("Package.swift"))?;

    // Copy an empty package to folders we want ignored
    let ignored_package_folders = ["target"];
    for path in ignored_package_folders {
        copy(
            apple_dir.join("Debug-Empty-Package.swift"),
            root_dir.join(path).join("Package.swift"),
        )?;
    }

    // cleaning up the intermediate data
    remove_dir_all(headers_dir.as_path())?;

    if let Some(path) = components_path {
        println!("-- Copying MatrixSDKFFI.xcframework to {path:?}");
        let framework_target = path.join("MatrixSDKFFI.xcframework");
        let swift_target = path.join("Sources/MatrixRustSDK");
        if framework_target.exists() {
            remove_dir_all(framework_target.as_path())?;
        }
        if swift_target.exists() {
            remove_dir_all(swift_target.as_path())?;
        }
        create_dir_all(framework_target.as_path())?;
        create_dir_all(swift_target.as_path())?;

        let copy_options = fs_extra::dir::CopyOptions { content_only: true, ..Default::default() };

        fs_extra::dir::copy(xcframework_path.as_path(), framework_target.as_path(), &copy_options)?;
        fs_extra::dir::copy(swift_dir.as_path(), swift_target.as_path(), &copy_options)?;
    }

    println!("-- All done and hunky dory. Enjoy!");

    Ok(())
}
