use std::{
    collections::HashMap,
    fs::{copy, create_dir_all, remove_dir_all, remove_file, rename},
};

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

/// A specific build target supported by the SDK.
struct Target {
    triple: &'static str,
    platform: Platform,
    description: &'static str,
}

/// The platform for which a particular target can run on.
#[derive(Hash, PartialEq, Eq, Clone)]
enum Platform {
    Macos,
    Ios,
    IosSimulator,
}

impl Platform {
    /// The human-readable name of the platform.
    fn as_str(&self) -> &str {
        match self {
            Platform::Macos => "macOS",
            Platform::Ios => "iOS",
            Platform::IosSimulator => "iOS Simulator",
        }
    }

    /// The name of the library for the platform once all architectures are
    /// lipo'd together.
    fn lib_name(&self) -> &str {
        match self {
            Platform::Macos => "libmatrix_sdk_ffi_macos.a",
            Platform::Ios => "libmatrix_sdk_ffi_ios.a",
            Platform::IosSimulator => "libmatrix_sdk_ffi_iossimulator.a",
        }
    }
}

/// The base name of the FFI library.
const FFI_LIBRARY_NAME: &str = "libmatrix_sdk_ffi.a";
/// The list of targets supported by the SDK.
const TARGETS: &[Target] = &[
    Target { triple: "aarch64-apple-ios", platform: Platform::Ios, description: "iOS" },
    Target {
        triple: "aarch64-apple-darwin",
        platform: Platform::Macos,
        description: "macOS (Apple Silicon)",
    },
    Target {
        triple: "x86_64-apple-darwin",
        platform: Platform::Macos,
        description: "macOS (Intel)",
    },
    Target {
        triple: "aarch64-apple-ios-sim",
        platform: Platform::IosSimulator,
        description: "iOS Simulator (Apple Silicon)",
    },
    Target {
        triple: "x86_64-apple-ios",
        platform: Platform::IosSimulator,
        description: "iOS Simulator (Intel) ",
    },
];

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

fn build_path_for_target(target: &Target, profile: &str) -> Result<Utf8PathBuf> {
    // The builtin dev profile has its files stored under target/debug, all
    // other targets have matching directory names
    let profile_dir_name = if profile == "dev" { "debug" } else { profile };
    Ok(workspace::target_path()?.join(target.triple).join(profile_dir_name).join(FFI_LIBRARY_NAME))
}

fn build_targets(
    targets: Vec<&Target>,
    profile: &str,
    sequentially: bool,
) -> Result<HashMap<Platform, Vec<Utf8PathBuf>>> {
    if sequentially {
        for target in &targets {
            let triple = target.triple;

            println!("-- Building for {}", target.description);
            cmd!("rustup run stable cargo build -p matrix-sdk-ffi --target {triple} --profile {profile}")
                .run()?;
        }
    } else {
        let triples = &targets.iter().map(|target| target.triple).collect::<Vec<_>>();
        let mut cmd = cmd!("rustup run stable cargo build -p matrix-sdk-ffi");
        for triple in triples {
            cmd = cmd.arg("--target").arg(triple);
        }
        cmd = cmd.arg("--profile").arg(profile);

        println!("-- Building for {} targets", triples.len());
        cmd.run()?;
    }

    // a hashmap of platform to array, where each array contains all the paths for
    // that platform.
    let mut platform_build_paths = HashMap::new();
    for target in targets {
        let path = build_path_for_target(target, profile)?;
        let paths = platform_build_paths.entry(target.platform.clone()).or_insert_with(Vec::new);
        paths.push(path);
    }

    Ok(platform_build_paths)
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

    let targets = if let Some(triple) = only_target {
        let target = TARGETS.iter().find(|t| t.triple == triple).expect("Invalid target specified");
        vec![target]
    } else {
        TARGETS.iter().collect()
    };

    let platform_build_paths = build_targets(targets, profile, sequentially)?;

    let mut libs = Vec::new();
    for platform in platform_build_paths.keys() {
        println!("-- Running Lipo for {}", platform.as_str());
        let paths = platform_build_paths.get(platform).unwrap();
        let output_path = generated_dir.join(platform.lib_name());
        let mut cmd = cmd!("lipo -create");
        for path in paths {
            cmd = cmd.arg(path);
        }
        cmd = cmd.arg("-output").arg(&output_path);
        cmd.run()?;
        libs.push(output_path);
    }

    println!("-- Generating uniffi files");
    let uniffi_lib_path = platform_build_paths.values().next().unwrap().first().unwrap().clone();
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
