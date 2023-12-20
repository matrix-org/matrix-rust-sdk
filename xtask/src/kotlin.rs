use std::fs::create_dir_all;

use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Subcommand, ValueEnum};
use uniffi_bindgen::{bindings::TargetLanguage, library_mode::generate_bindings};
use xshell::{cmd, pushd};

use crate::{workspace, Result};

struct PackageValues {
    name: &'static str,
}

#[derive(ValueEnum, Clone)]
enum Package {
    CryptoSDK,
    FullSDK,
}

impl Package {
    fn values(self) -> PackageValues {
        match self {
            Package::CryptoSDK => PackageValues { name: "matrix-sdk-crypto-ffi" },
            Package::FullSDK => PackageValues { name: "matrix-sdk-ffi" },
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

        /// Move the generated files into the given src directory
        #[clap(long)]
        src_dir: Utf8PathBuf,
    },
}

impl KotlinArgs {
    pub fn run(self) -> Result<()> {
        let _p = pushd(workspace::root_path()?)?;

        match self.cmd {
            KotlinCommand::BuildAndroidLibrary {
                release,
                profile,
                src_dir,
                only_target,
                package,
            } => {
                let profile = profile.as_deref().unwrap_or(if release { "release" } else { "dev" });
                build_android_library(profile, only_target, src_dir, package)
            }
        }
    }
}

fn build_android_library(
    profile: &str,
    only_target: Option<String>,
    src_dir: Utf8PathBuf,
    package: Package,
) -> Result<()> {
    let package_values = package.values();
    let package_name = package_values.name;

    let jni_libs_dir = src_dir.join("jniLibs");
    let jni_libs_dir_str = jni_libs_dir.as_str();

    let kotlin_generated_dir = src_dir.join("kotlin");
    create_dir_all(kotlin_generated_dir.clone())?;

    let uniffi_lib_path = if let Some(target) = only_target {
        println!("-- Building for {target} [1/1]");
        build_for_android_target(target.as_str(), profile, jni_libs_dir_str, package_name)?
    } else {
        println!("-- Building for x86_64-linux-android[1/4]");
        build_for_android_target("x86_64-linux-android", profile, jni_libs_dir_str, package_name)?;
        println!("-- Building for aarch64-linux-android[2/4]");
        build_for_android_target("aarch64-linux-android", profile, jni_libs_dir_str, package_name)?;
        println!("-- Building for armv7-linux-androideabi[3/4]");
        build_for_android_target(
            "armv7-linux-androideabi",
            profile,
            jni_libs_dir_str,
            package_name,
        )?;
        println!("-- Building for i686-linux-android[4/4]");
        build_for_android_target("i686-linux-android", profile, jni_libs_dir_str, package_name)?
    };

    println!("-- Generate uniffi files");
    generate_uniffi_bindings(&uniffi_lib_path, &kotlin_generated_dir)?;

    println!("-- All done and hunky dory. Enjoy!");
    Ok(())
}

fn generate_uniffi_bindings(library_path: &Utf8Path, ffi_generated_dir: &Utf8Path) -> Result<()> {
    println!("-- library_path = {library_path}");

    generate_bindings(
        library_path,
        None,
        &[TargetLanguage::Kotlin],
        None,
        ffi_generated_dir,
        false,
    )?;
    Ok(())
}

fn build_for_android_target(
    target: &str,
    profile: &str,
    dest_dir: &str,
    package_name: &str,
) -> Result<Utf8PathBuf> {
    cmd!("cargo ndk --target {target} -o {dest_dir} build --profile {profile} -p {package_name}")
        .run()?;

    // The builtin dev profile has its files stored under target/debug, all
    // other targets have matching directory names
    let profile_dir_name = if profile == "dev" { "debug" } else { profile };
    let package_camel = package_name.replace('-', "_");
    let lib_name = format!("lib{package_camel}.so");
    Ok(workspace::target_path()?.join(target).join(profile_dir_name).join(lib_name))
}
