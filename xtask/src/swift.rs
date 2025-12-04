use std::{
    collections::HashMap,
    fs::{copy, create_dir_all, remove_dir_all, remove_file, rename},
};

use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Subcommand};
use uniffi_bindgen::bindings::{GenerateOptions, TargetLanguage, generate};
use xshell::cmd;

use crate::{Result, sh, workspace};

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

        /// Build the given target. This option can be specified multiple times
        /// to build more than one. Omitting this option will build all
        /// supported targets.
        #[clap(long)]
        target: Option<Vec<String>>,

        /// Includes the Tier 3 targets (such as watchOS) when building all
        /// supported targets. Requires a nightly toolchain with the `rust-src`
        /// component installed.
        #[clap(long)]
        tier3_targets: bool,

        /// Move the generated xcframework and swift sources into the given
        /// components-folder
        #[clap(long)]
        components_path: Option<Utf8PathBuf>,

        /// The iOS deployment target to use when building the framework.
        ///
        /// Defaults to not being set, which implies that the build will use the
        /// default values provided by the Rust and Xcode toolchains.
        #[clap(long)]
        ios_deployment_target: Option<String>,

        /// The wachOS deployment target to use when building the framework.
        ///
        /// Defaults to not being set, which implies that the build will use the
        /// default values provided by the Rust and Xcode toolchains.
        #[clap(long)]
        watchos_deployment_target: Option<String>,

        /// Build the targets one by one instead of passing all of them
        /// to cargo in one go, which makes it hang on lesser devices like plain
        /// Apple Silicon M1s
        #[clap(long)]
        sequentially: bool,
    },
}

impl SwiftArgs {
    pub fn run(self) -> Result<()> {
        let sh = sh();
        let _p = sh.push_dir(workspace::root_path()?);

        match self.cmd {
            SwiftCommand::BuildLibrary => build_library(),
            SwiftCommand::BuildFramework {
                release,
                profile,
                target: targets,
                tier3_targets,
                components_path,
                ios_deployment_target,
                watchos_deployment_target,
                sequentially,
            } => {
                // The dev profile seems to cause crashes on some platforms so we default to
                // reldbg (https://github.com/matrix-org/matrix-rust-sdk/issues/4009)
                let profile =
                    profile.as_deref().unwrap_or(if release { "release" } else { "reldbg" });
                build_xcframework(
                    profile,
                    targets,
                    tier3_targets,
                    components_path,
                    sequentially,
                    ios_deployment_target.as_deref(),
                    watchos_deployment_target.as_deref(),
                )
            }
        }
    }
}

/// A specific build target supported by the SDK.
struct Target {
    triple: &'static str,
    platform: Platform,
    status: TargetStatus,
    description: &'static str,
}

#[derive(Hash, PartialEq, Eq, Clone)]
enum TargetStatus {
    /// A tier 1 or 2 target that can be built with stable Rust.
    TopTier,
    /// A tier 3 target that requires nightly Rust and `-Zbuild-std`.
    Tier3,
}

/// The platform for which a particular target can run on.
#[derive(Hash, PartialEq, Eq, Clone)]
enum Platform {
    Macos,
    Ios,
    IosSimulator,
    Watchos,
    WatchosSimulator,
}

impl Platform {
    /// The human-readable name of the platform.
    fn as_str(&self) -> &str {
        match self {
            Platform::Macos => "macOS",
            Platform::Ios => "iOS",
            Platform::IosSimulator => "iOS Simulator",
            Platform::Watchos => "watchOS",
            Platform::WatchosSimulator => "watchOS Simulator",
        }
    }

    /// The name of the subfolder in which to place the library for the platform
    /// once all architectures are lipo'd together.
    fn lib_folder_name(&self) -> &str {
        match self {
            Platform::Macos => "macos",
            Platform::Ios => "ios",
            Platform::IosSimulator => "ios-simulator",
            Platform::Watchos => "watchos",
            Platform::WatchosSimulator => "watchos-simulator",
        }
    }
}
/// The base name of the FFI library.
const FFI_LIBRARY_NAME: &str = "libmatrix_sdk_ffi.a";

/// The features enabled for the FFI library.
const FFI_FEATURES: &str = "native-tls,sentry";

/// The list of targets supported by the SDK.
const TARGETS: &[Target] = &[
    Target {
        triple: "aarch64-apple-ios",
        platform: Platform::Ios,
        status: TargetStatus::TopTier,
        description: "iOS",
    },
    Target {
        triple: "aarch64-apple-darwin",
        platform: Platform::Macos,
        status: TargetStatus::TopTier,
        description: "macOS (Apple Silicon)",
    },
    Target {
        triple: "x86_64-apple-darwin",
        platform: Platform::Macos,
        status: TargetStatus::TopTier,
        description: "macOS (Intel)",
    },
    Target {
        triple: "aarch64-apple-ios-sim",
        platform: Platform::IosSimulator,
        status: TargetStatus::TopTier,
        description: "iOS Simulator (Apple Silicon)",
    },
    Target {
        triple: "x86_64-apple-ios",
        platform: Platform::IosSimulator,
        status: TargetStatus::TopTier,
        description: "iOS Simulator (Intel) ",
    },
    Target {
        triple: "aarch64-apple-watchos",
        platform: Platform::Watchos,
        status: TargetStatus::Tier3,
        description: "watchOS (ARM64)",
    },
    Target {
        triple: "arm64_32-apple-watchos",
        platform: Platform::Watchos,
        status: TargetStatus::Tier3,
        description: "watchOS (ARM64_32)",
    },
    Target {
        triple: "aarch64-apple-watchos-sim",
        platform: Platform::WatchosSimulator,
        status: TargetStatus::Tier3,
        description: "watchOS Simulator (ARM64)",
    },
    Target {
        triple: "x86_64-apple-watchos-sim",
        platform: Platform::WatchosSimulator,
        status: TargetStatus::Tier3,
        description: "watchOS Simulator (Intel)",
    },
];

fn build_library() -> Result<()> {
    println!("Running debug library build.");

    let root_directory = workspace::root_path()?;
    let target_directory = workspace::target_path()?;
    let ffi_directory = root_directory.join("bindings/apple/generated/matrix_sdk_ffi");
    let lib_output_dir = target_directory.join("debug");

    create_dir_all(ffi_directory.as_path())?;

    let sh = sh();
    cmd!(sh, "rustup run stable cargo build -p matrix-sdk-ffi --features {FFI_FEATURES}").run()?;

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
    generate(GenerateOptions {
        languages: vec![TargetLanguage::Swift],
        source: library_path.to_path_buf(),
        out_dir: ffi_directory.to_path_buf(),
        ..GenerateOptions::default()
    })?;
    Ok(())
}

fn build_xcframework(
    profile: &str,
    targets: Option<Vec<String>>,
    tier3_targets: bool,
    components_path: Option<Utf8PathBuf>,
    sequentially: bool,
    ios_deployment_target: Option<&str>,
    watchos_deployment_target: Option<&str>,
) -> Result<()> {
    let root_dir = workspace::root_path()?;
    let apple_dir = root_dir.join("bindings/apple");
    let generated_dir = apple_dir.join("generated");

    // Cleanup destination folder
    let _ = remove_dir_all(&generated_dir);
    create_dir_all(&generated_dir)?;

    let headers_dir = generated_dir.join("headers");
    // Use a subdirectory to fix conflicts with other UniFFI libraries.
    let headers_module_dir = headers_dir.join("MatrixSDKFFI");
    let swift_dir = generated_dir.join("swift");
    create_dir_all(headers_module_dir.clone())?;
    create_dir_all(swift_dir.clone())?;

    let targets = if let Some(triples) = targets {
        triples
            .iter()
            .map(|t| {
                TARGETS.iter().find(|target| target.triple == *t).expect("Invalid target specified")
            })
            .collect()
    } else if tier3_targets {
        TARGETS.iter().collect()
    } else {
        TARGETS.iter().filter(|target| target.status == TargetStatus::TopTier).collect()
    };

    let platform_build_paths = build_targets(
        targets,
        profile,
        sequentially,
        ios_deployment_target,
        watchos_deployment_target,
    )?;
    let libs = lipo_platform_libraries(&platform_build_paths, &generated_dir)?;

    println!("-- Generating uniffi files");
    let uniffi_lib_path = platform_build_paths.values().next().unwrap().first().unwrap().clone();
    generate_uniffi(&uniffi_lib_path, &generated_dir)?;

    move_files("h", &generated_dir, &headers_module_dir)?;
    consolidate_modulemap_files(&generated_dir, &headers_module_dir)?;

    move_files("swift", &generated_dir, &swift_dir)?;
    apply_uniffi_mocks_workaround(&swift_dir)?;

    println!("-- Generating MatrixSDKFFI.xcframework framework");
    let xcframework_path = generated_dir.join("MatrixSDKFFI.xcframework");
    if xcframework_path.exists() {
        remove_dir_all(&xcframework_path)?;
    }
    let sh = sh();
    let mut cmd = cmd!(sh, "xcodebuild -create-xcframework");
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

/// Builds the SDK for the specified targets and profile.
fn build_targets(
    targets: Vec<&Target>,
    profile: &str,
    sequentially: bool,
    ios_deployment_target: Option<&str>,
    watchos_deployment_target: Option<&str>,
) -> Result<HashMap<Platform, Vec<Utf8PathBuf>>> {
    let sh = sh();

    // Note: `push_env` stores environment variables and returns a RAII guard that
    // will restore the environment variable to its previous value when dropped.
    let _env_guard1 =
        sh.push_env("CARGO_TARGET_AARCH64_APPLE_IOS_RUSTFLAGS", "-Clinker=/usr/bin/clang");
    let _env_guard2 = sh.push_env("AARCH64_APPLE_IOS_CC", "/usr/bin/clang");
    let _env_guard3 =
        ios_deployment_target.map(|target| sh.push_env("IPHONEOS_DEPLOYMENT_TARGET", target));
    let _env_guard4 =
        watchos_deployment_target.map(|target| sh.push_env("WATCHOS_DEPLOYMENT_TARGET", target));

    if sequentially {
        for target in &targets {
            let triple = target.triple;

            println!("-- Building for {}", target.description);
            if target.status == TargetStatus::TopTier {
                cmd!(sh, "rustup run stable cargo build -p matrix-sdk-ffi --target {triple} --profile {profile} --features {FFI_FEATURES}")
                    .run()?;
            } else {
                cmd!(sh, "rustup run nightly cargo build -p matrix-sdk-ffi -Zbuild-std --target {triple} --profile {profile} --features {FFI_FEATURES}")
                    .run()?;
            }
        }
    } else {
        let (stable_targets, tier3_targets): (Vec<&Target>, Vec<&Target>) =
            targets.iter().partition(|t| t.status == TargetStatus::TopTier);

        if !stable_targets.is_empty() {
            let triples = stable_targets.iter().map(|target| target.triple).collect::<Vec<_>>();
            let mut cmd = cmd!(sh, "rustup run stable cargo build -p matrix-sdk-ffi");
            for triple in &triples {
                cmd = cmd.arg("--target").arg(triple);
            }
            cmd = cmd.arg("--profile").arg(profile).arg("--features").arg(FFI_FEATURES);

            println!("-- Building for {} targets", triples.len());
            cmd.run()?;
        }

        if !tier3_targets.is_empty() {
            let triples = tier3_targets.iter().map(|target| target.triple).collect::<Vec<_>>();
            let mut cmd = cmd!(sh, "rustup run nightly cargo build -p matrix-sdk-ffi -Zbuild-std");
            for triple in &triples {
                cmd = cmd.arg("--target").arg(triple);
            }
            cmd = cmd.arg("--profile").arg(profile).arg("--features").arg(FFI_FEATURES);

            println!("-- Building for {} targets with nightly -Zbuild-std", triples.len());
            cmd.run()?;
        }
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

/// The path of the built library for a specific target and profile.
fn build_path_for_target(target: &Target, profile: &str) -> Result<Utf8PathBuf> {
    // The builtin dev profile has its files stored under target/debug, all
    // other targets have matching directory names
    let profile_dir_name = if profile == "dev" { "debug" } else { profile };
    Ok(workspace::target_path()?.join(target.triple).join(profile_dir_name).join(FFI_LIBRARY_NAME))
}

/// Lipo's together the libraries for each platform into a single library.
fn lipo_platform_libraries(
    platform_build_paths: &HashMap<Platform, Vec<Utf8PathBuf>>,
    generated_dir: &Utf8Path,
) -> Result<Vec<Utf8PathBuf>> {
    let mut libs = Vec::new();
    let sh = sh();
    for platform in platform_build_paths.keys() {
        let paths = platform_build_paths.get(platform).unwrap();

        if paths.len() == 1 {
            libs.push(paths[0].clone());
            continue;
        }

        let output_folder = generated_dir.join("lipo").join(platform.lib_folder_name());
        create_dir_all(&output_folder)?;

        let output_path = output_folder.join(FFI_LIBRARY_NAME);
        let mut cmd = cmd!(sh, "lipo -create");
        for path in paths {
            cmd = cmd.arg(path);
        }
        cmd = cmd.arg("-output").arg(&output_path);

        println!("-- Running Lipo for {}", platform.as_str());
        cmd.run()?;

        libs.push(output_path);
    }
    Ok(libs)
}

/// Moves all files of the specified file extension from one directory into
/// another.
fn move_files(extension: &str, source: &Utf8Path, destination: &Utf8Path) -> Result<()> {
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
fn consolidate_modulemap_files(source: &Utf8Path, destination: &Utf8Path) -> Result<()> {
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

// Temporary workaround for https://github.com/mozilla/uniffi-rs/issues/2717
fn apply_uniffi_mocks_workaround(swift_dir: &Utf8Path) -> Result<()> {
    let regex = regex::Regex::new(r#"(deinit\s*\{\n)(\s*try!\s*rustCall)"#)?;

    for entry in swift_dir.read_dir_utf8()? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            let path = entry.path();
            if path.extension() == Some("swift") {
                let mut contents = std::fs::read_to_string(path)?;
                let new_contents =
                    regex.replace_all(&contents, "$1        guard handle != 0 else { return }\n$2");
                if new_contents != contents {
                    contents = new_contents.to_string();
                    std::fs::write(path, contents)?;
                }
            }
        }
    }

    Ok(())
}
