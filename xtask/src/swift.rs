use std::fs::{copy, create_dir_all, remove_dir_all, remove_file, rename};

use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Subcommand};
use uniffi_bindgen::{bindings::TargetLanguage, library_mode::generate_bindings};
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
        components_path: Option<Utf8PathBuf>,

        /// Build the targets one by one instead of passing all of them
        /// to cargo in one go, which makes it hang on lesser devices like plain
        /// Apple Silicon M1s
        #[clap(long)]
        sequentially: bool,
    },
}

impl SwiftArgs {
    pub fn run(self) -> Result<()> {
        let _p = pushd(workspace::root_path()?)?;

        match self.cmd {
            SwiftCommand::BuildLibrary => build_library(),
            SwiftCommand::BuildFramework {
                release,
                profile,
                components_path,
                only_target,
                sequentially,
            } => {
                let profile = profile.as_deref().unwrap_or(if release { "release" } else { "dev" });
                build_xcframework(profile, only_target, components_path, sequentially)
            }
        }
    }
}

const FFI_LIBRARY_NAME: &str = "libmatrix_sdk_ffi.a";

fn build_library() -> Result<()> {
    println!("Running debug library build.");

    let root_directory = workspace::root_path()?;
    let target_directory = workspace::target_path()?;
    let ffi_directory = root_directory.join("bindings/apple/generated/matrix_sdk_ffi");
    let lib_output_dir = target_directory.join("debug");

    create_dir_all(ffi_directory.as_path())?;

    cmd!("rustup run stable cargo build -p matrix-sdk-ffi").run()?;

    rename(lib_output_dir.join(FFI_LIBRARY_NAME), ffi_directory.join(FFI_LIBRARY_NAME))?;
    let swift_directory = root_directory.join("bindings/apple/generated/swift");
    create_dir_all(swift_directory.as_path())?;

    generate_uniffi(&ffi_directory.join(FFI_LIBRARY_NAME), &ffi_directory)?;

    let module_map_file = ffi_directory.join("module.modulemap");
    if module_map_file.exists() {
        remove_file(module_map_file.as_path())?;
    }

    consolidate_modulemap_files(&ffi_directory, &ffi_directory)?;
    move_files("swift", &ffi_directory, &swift_directory)?;
    Ok(())
}

fn generate_uniffi(library_path: &Utf8Path, ffi_directory: &Utf8Path) -> Result<()> {
    generate_bindings(library_path, None, &[TargetLanguage::Swift], None, ffi_directory, false)?;
    Ok(())
}

fn build_path_for_target(target: &str, profile: &str) -> Result<Utf8PathBuf> {
    // The builtin dev profile has its files stored under target/debug, all
    // other targets have matching directory names
    let profile_dir_name = if profile == "dev" { "debug" } else { profile };
    Ok(workspace::target_path()?.join(target).join(profile_dir_name).join(FFI_LIBRARY_NAME))
}

fn build_for_target(target: &str, profile: &str) -> Result<Utf8PathBuf> {
    cmd!("rustup run stable cargo build -p matrix-sdk-ffi --target {target} --profile {profile}")
        .run()?;

    build_path_for_target(target, profile)
}

fn build_xcframework(
    profile: &str,
    only_target: Option<String>,
    components_path: Option<Utf8PathBuf>,
    sequentially: bool,
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

    let (libs, uniffi_lib_path) = if let Some(target) = only_target {
        println!("-- Building for {target} 1/1");

        cmd!(
            "rustup run stable cargo build -p matrix-sdk-ffi --target {target} --profile {profile}"
        )
        .run()?;

        let build_path = build_path_for_target(target.as_str(), profile)?;

        (vec![build_path.clone()], build_path)
    } else if sequentially {
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
        // macOS
        let lipo_target_macos = generated_dir.join("libmatrix_sdk_ffi_macos.a");
        cmd!(
            "lipo -create {darwin_x86_path} {darwin_arm_path}
            -output {lipo_target_macos}"
        )
        .run()?;

        println!("-- Running Lipo for iOS Simulator [2/2]");
        // iOS Simulator
        let lipo_target_sim = generated_dir.join("libmatrix_sdk_ffi_iossimulator.a");
        cmd!(
            "lipo -create {ios_sim_arm_path} {ios_sim_x86_path}
            -output {lipo_target_sim}"
        )
        .run()?;

        (vec![lipo_target_macos, lipo_target_sim, ios_path], darwin_x86_path)
    } else {
        println!("-- Building for 5 targets");

        cmd!(
            "rustup run stable cargo build -p matrix-sdk-ffi
            --target aarch64-apple-ios
            --target aarch64-apple-darwin
            --target x86_64-apple-darwin
            --target aarch64-apple-ios-sim
            --target x86_64-apple-ios
            --profile {profile}"
        )
        .run()?;

        let ios_path = build_path_for_target("aarch64-apple-ios", profile)?;
        let darwin_arm_path = build_path_for_target("aarch64-apple-darwin", profile)?;
        let darwin_x86_path = build_path_for_target("x86_64-apple-darwin", profile)?;
        let ios_sim_arm_path = build_path_for_target("aarch64-apple-ios-sim", profile)?;
        let ios_sim_x86_path = build_path_for_target("x86_64-apple-ios", profile)?;

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
    generate_uniffi(&uniffi_lib_path, &generated_dir)?;

    move_files("h", &generated_dir, &headers_dir)?;
    consolidate_modulemap_files(&generated_dir, &headers_dir)?;

    move_files("swift", &generated_dir, &swift_dir)?;

    println!("-- Generating MatrixSDKFFI.xcframework framework");
    let xcframework_path = generated_dir.join("MatrixSDKFFI.xcframework");
    if xcframework_path.exists() {
        remove_dir_all(&xcframework_path)?;
    }
    let mut cmd = cmd!("xcodebuild -create-xcframework");
    for p in libs {
        cmd = cmd.arg("-library").arg(p).arg("-headers").arg(&headers_dir)
    }
    cmd.arg("-output").arg(&xcframework_path).run()?;

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
        println!("-- Copying MatrixSDKFFI.xcframework to {path}");
        let framework_target = path.join("MatrixSDKFFI.xcframework");
        let swift_target = path.join("Sources/MatrixRustSDK");
        if framework_target.exists() {
            remove_dir_all(&framework_target)?;
        }
        if swift_target.exists() {
            remove_dir_all(&swift_target)?;
        }
        create_dir_all(&framework_target)?;
        create_dir_all(&swift_target)?;

        let copy_options = fs_extra::dir::CopyOptions { content_only: true, ..Default::default() };

        fs_extra::dir::copy(&xcframework_path, &framework_target, &copy_options)?;
        fs_extra::dir::copy(&swift_dir, &swift_target, &copy_options)?;
    }

    println!("-- All done and hunky dory. Enjoy!");

    Ok(())
}

/// Moves all files of the specified file extension from one directory into
/// another.
fn move_files(extension: &str, source: &Utf8PathBuf, destination: &Utf8PathBuf) -> Result<()> {
    for entry in source.read_dir_utf8()? {
        let entry = entry?;

        if entry.file_type()?.is_file() {
            let path = entry.path();
            if path.extension() == Some(extension) {
                let file_name = path.file_name().expect("Failed to get file name");
                rename(path, destination.join(file_name)).expect("Failed to move swift file");
            }
        }
    }
    Ok(())
}

/// Consolidates the contents of each modulemap file found in the source
/// directory into a single `module.modulemap` file in the destination
/// directory.
fn consolidate_modulemap_files(source: &Utf8PathBuf, destination: &Utf8PathBuf) -> Result<()> {
    let mut modulemap = String::new();
    for entry in source.read_dir_utf8()? {
        let entry = entry?;

        if entry.file_type()?.is_file() {
            let path = entry.path();
            if path.extension() == Some("modulemap") {
                let contents = std::fs::read_to_string(path)?;
                modulemap.push_str(&contents);
                modulemap.push_str("\n\n");
                remove_file(path)?;
            }
        }
    }

    std::fs::write(destination.join("module.modulemap"), modulemap)?;
    Ok(())
}
