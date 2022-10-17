use std::fs;

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
    BuildFramework,
}

impl SwiftArgs {
    pub fn run(self) -> Result<()> {
        let _p = pushd(&workspace::root_path()?)?;

        match self.cmd {
            SwiftCommand::BuildLibrary => build_library(),
            SwiftCommand::BuildFramework => build_xcframework(),
        }
    }
}

fn build_library() -> Result<()> {
    println!("Running debug library build.");

    let root_directory = workspace::root_path()?;
    let target_directory = root_directory.join("target");
    let ffi_directory = root_directory.join("bindings/apple/generated/matrix_sdk_ffi");
    let swift_directory = root_directory.join("bindings/apple/generated/swift");

    let release_type = "debug";
    let static_lib_filename = "libmatrix_sdk_ffi.a";

    fs::create_dir_all(ffi_directory.as_path())?;
    fs::create_dir_all(swift_directory.as_path())?;

    cmd!("cargo build -p matrix-sdk-ffi").run()?;

    fs::rename(
        target_directory.join(release_type).join(static_lib_filename),
        ffi_directory.join(static_lib_filename),
    )?;

    cmd!(
        "uniffi-bindgen generate
    			--language swift
    			--lib-file {ffi_directory}/{static_lib_filename}
    			--out-dir {ffi_directory}
				{root_directory}/bindings/matrix-sdk-ffi/src/api.udl"
    )
    .run()?;

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

fn build_xcframework() -> Result<()> {
    println!("XCFramework not yet implemented.");
    Ok(())
}
