use std::{
    fs::{create_dir_all, remove_dir_all, rename},
    path::{Path, PathBuf},
};

use clap::{Args, Subcommand, ValueEnum};
use xshell::{cmd, pushd};

use crate::{workspace, Result};

struct PackageValues {
    name: &'static str,
    udl_path: &'static str,
    gradle_path: &'static str,
    gradle_module: &'static str,
    aar_name: &'static str,
}

#[derive(ValueEnum, Clone)]
enum Package {
    CryptoSDK,
    FullSDK,
}

impl Package {
    fn values(self) -> PackageValues {
        match self {
            Package::CryptoSDK => PackageValues { 
                name: "matrix-sdk-crypto-ffi",
                udl_path : "bindings/matrix-sdk-crypto-ffi/src/olm.udl",
                gradle_path : "crypto/crypto-android",
                gradle_module : "crypto:crypto-android:",
                aar_name : "crypto-android",
            },
            Package::FullSDK => PackageValues { 
                name: "matrix-sdk-ffi",
                udl_path : "bindings/matrix-sdk-ffi/src/api.udl",
                gradle_path : "sdk/sdk-android",
                gradle_module : ":sdk:sdk-android:",
                aar_name : "sdk-android",
            },
        }
    }
}

#[derive(Args)]
pub struct KotlinArgs {
    #[clap(subcommand)]
    cmd: KotlinCommand,
}

#[derive(Subcommand)]
enum KotlinCommand {
    /// Builds the SDK for Android as an AAR.
    BuildAndroidLibrary {
        #[clap(value_enum, long)]
        package: Package,
        /// Build with the release profile
        #[clap(long)]
        release: bool,

        /// Build with a custom profile, takes precedence over `--release`
        #[clap(long)]
        profile: Option<String>,

        /// Build the given target only
        #[clap(long)]
        only_target: Option<String>,

        /// Move the generated aar into the given path
        #[clap(long)]
        aar_path: Option<PathBuf>,
    },
}

impl KotlinArgs {
    pub fn run(self) -> Result<()> {
        let _p = pushd(workspace::root_path()?)?;

        match self.cmd {
            KotlinCommand::BuildAndroidLibrary { release, profile, aar_path, only_target,package } => {
                let profile = profile.as_deref().unwrap_or(if release { "release" } else { "dev" });
                build_android_library(profile, only_target, aar_path, package)
            }
        }
    }
}

fn build_android_library(
    profile: &str,
    only_target: Option<String>,
    aar_path: Option<PathBuf>,
    package: Package,
) -> Result<()> {

    let root_dir = workspace::root_path()?;
    
    let package_values = package.values();
    let package_name = package_values.name;
    let gradle_path = package_values.gradle_path;
    let udl_path = root_dir.join(package_values.udl_path);
    let kotlin_dir = root_dir.join("bindings/kotlin");

    let gradle_root_dir = kotlin_dir.join(gradle_path);
    let gradle_target_dir = gradle_root_dir.join("src/main/jniLibs");
    let gradle_build_dir = gradle_root_dir.join("build");

    let build_variant = get_build_variant_values(profile).0;
    let gradle_generated_dir_path = format!("generated/source/{build_variant}");
    let gradle_generated_dir = gradle_build_dir.join(gradle_generated_dir_path);
    create_dir_all(gradle_generated_dir.clone())?;

    let sdk_target_dir_str = gradle_target_dir.to_str().unwrap();

    let uniffi_lib_path = if let Some(target) = only_target {
        println!("-- Building for {target} [1/1]");
        build_for_android_target(target.as_str(), profile,sdk_target_dir_str, package_name)?
    } else {
        println!("-- Building for x86_64-linux-android[1/4]");
        build_for_android_target("x86_64-linux-android", profile,sdk_target_dir_str, package_name)?;
        println!("-- Building for aarch64-linux-android[2/4]");
        build_for_android_target("aarch64-linux-android", profile,sdk_target_dir_str, package_name)?;
        println!("-- Building for armv7-linux-androideabi[3/4]");
        build_for_android_target("armv7-linux-androideabi", profile,sdk_target_dir_str, package_name)?;
        println!("-- Building for i686-linux-android[4/4]");
        build_for_android_target("i686-linux-android", profile,sdk_target_dir_str, package_name)?
    };

    println!("-- Generate uniffi files");
    generate_uniffi_bindings(&udl_path, &uniffi_lib_path, &gradle_generated_dir)?;

    println!("-- Building aar");
    build_android_aar(profile, &kotlin_dir, &gradle_build_dir, aar_path, &package_values)?;

    println!("-- Cleaning up temporary files");
    //remove_dir_all(gradle_build_dir.as_path())?;
    remove_dir_all(gradle_target_dir.as_path())?;

    println!("-- All done and hunky dory. Enjoy!");
    Ok(())
}

fn generate_uniffi_bindings(udl_path: &Path, library_path: &Path, ffi_generated_dir: &Path) -> Result<()> {
    println!("-- library_path = {}", library_path.to_string_lossy());
    let udl_file = camino::Utf8Path::from_path(udl_path).unwrap();
    let out_dir_overwrite = camino::Utf8Path::from_path(ffi_generated_dir).unwrap();
    let library_file = camino::Utf8Path::from_path(library_path).unwrap();

    uniffi_bindgen::generate_bindings(
        udl_file,
        None,
        vec!["kotlin"],
        Some(out_dir_overwrite),
        Some(library_file),
        false,
    )?;
    Ok(())
}

fn build_for_android_target(target: &str, profile: &str, dest_dir: &str, package_name: &str) -> Result<PathBuf> {

    cmd!("cargo ndk --target {target} -o {dest_dir} build --profile {profile} -p {package_name}").run()?;

    // The builtin dev profile has its files stored under target/debug, all
    // other targets have matching directory names
    let profile_dir_name = if profile == "dev" { "debug" } else { profile };
    let package_camel = package_name.replace("-","_");
    let lib_name = format!("lib{package_camel}.a");
    Ok(workspace::target_path()?.join(target).join(profile_dir_name).join(lib_name))
}

fn build_android_aar(profile: &str, kotlin_dir: &Path, build_dir: &Path, aar_path: Option<PathBuf>, package_values: &PackageValues) -> Result<()> {
    let _p = pushd(kotlin_dir)?;
    let (build_variant, build_variant_command) = get_build_variant_values(profile);
    let gradle_module = package_values.gradle_module;

    cmd!("./gradlew {gradle_module}{build_variant_command}").run()?;

    if let Some(path) = aar_path {
        let aar_name = package_values.aar_name;
        let full_aar_name = &format!("{aar_name}-{build_variant}.aar");
        println!("-- Copying aar to {path:?}");
        let aar_path  = &format!("outputs/aar/{full_aar_name}");
        rename(
            build_dir.join(aar_path),
            path.join(full_aar_name),
        )?;
    };
    Ok(())
}

fn get_build_variant_values(profile: &str)-> (&str, &str){
    if profile == "dev" {
        ("debug", "assembleDebug")
    } else { 
        ("release","assembleRelease")
    }
}
