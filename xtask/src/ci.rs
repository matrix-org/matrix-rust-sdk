use std::{collections::BTreeMap, env, path::PathBuf};

use clap::{Args, Subcommand};
use serde::Deserialize;
use xshell::{cmd, pushd};

use crate::{build_docs, DenyWarnings, Result};

#[derive(Args)]
pub struct CiArgs {
    #[clap(subcommand)]
    cmd: Option<CiCommand>,
}

#[derive(Subcommand)]
enum CiCommand {
    /// Check style
    Style,
    /// Check for typos
    Typos,
    /// Check clippy lints
    Clippy,
    /// Check documentation
    Docs,
    /// Run default tests
    Test,
    /// Run tests with a specific feature set
    TestFeatures {
        #[clap(subcommand)]
        cmd: Option<FeatureSet>,
    },
    /// Run tests for the appservice crate
    TestAppservice,
    /// Run checks for the wasm target
    Wasm {
        #[clap(subcommand)]
        cmd: Option<WasmFeatureSet>,
    },
}

#[derive(Subcommand, PartialEq, Eq, PartialOrd, Ord)]
enum FeatureSet {
    Default,
    NoEncryption,
    NoSled,
    NoEncryptionAndSled,
    SledCryptostore,
    RustlsTls,
    Markdown,
    Socks,
    SsoLogin,
}

#[derive(Subcommand, PartialEq, Eq, PartialOrd, Ord)]
#[allow(clippy::enum_variant_names)]
enum WasmFeatureSet {
    MatrixQrcode,
    MatrixSdkNoDefault,
    MatrixSdkBase,
    MatrixSdkCommon,
    MatrixSdkCrypto,
    MatrixSdkIndexeddbStores,
}

impl CiArgs {
    pub fn run(self) -> Result<()> {
        let _p = pushd(&workspace_root()?)?;

        match self.cmd {
            Some(cmd) => match cmd {
                CiCommand::Style => check_style(),
                CiCommand::Typos => check_typos(),
                CiCommand::Clippy => check_clippy(),
                CiCommand::Docs => check_docs(),
                CiCommand::Test => run_tests(),
                CiCommand::TestFeatures { cmd } => run_feature_tests(cmd),
                CiCommand::TestAppservice => run_appservice_tests(),
                CiCommand::Wasm { cmd } => run_wasm_checks(cmd),
            },
            None => {
                check_style()?;
                check_clippy()?;
                check_typos()?;
                check_docs()?;
                run_tests()?;
                run_feature_tests(None)?;
                run_appservice_tests()?;
                run_wasm_checks(None)?;

                Ok(())
            }
        }
    }
}

fn check_style() -> Result<()> {
    cmd!("rustup run nightly cargo fmt -- --check").run()?;
    Ok(())
}

fn check_typos() -> Result<()> {
    // FIXME: Print install instructions if command-not-found (needs an xshell
    //        change: https://github.com/matklad/xshell/issues/46)
    cmd!("typos").run()?;
    Ok(())
}

fn check_clippy() -> Result<()> {
    cmd!("rustup run nightly cargo clippy --all-targets -- -D warnings").run()?;
    cmd!(
        "rustup run nightly cargo clippy --all-targets
            --no-default-features --features native-tls,warp
            -- -D warnings"
    )
    .run()?;
    Ok(())
}

fn check_docs() -> Result<()> {
    build_docs([], DenyWarnings::Yes)
}

fn run_tests() -> Result<()> {
    cmd!("rustup run stable cargo test").run()?;
    cmd!("rustup run beta cargo test").run()?;
    Ok(())
}

fn run_feature_tests(cmd: Option<FeatureSet>) -> Result<()> {
    let args = BTreeMap::from([
        (FeatureSet::NoEncryption, "--no-default-features --features sled_state_store,native-tls"),
        (FeatureSet::NoSled, "--no-default-features --features encryption,native-tls"),
        (FeatureSet::NoEncryptionAndSled, "--no-default-features --features native-tls"),
        (
            FeatureSet::SledCryptostore,
            "--no-default-features --features encryption,sled_cryptostore,native-tls",
        ),
        (FeatureSet::RustlsTls, "--no-default-features --features rustls-tls"),
        (FeatureSet::Markdown, "--features markdown"),
        (FeatureSet::Socks, "--features socks"),
        (FeatureSet::SsoLogin, "--features sso_login"),
    ]);

    let run = |arg_set: &str| {
        cmd!("rustup run stable cargo test -p matrix-sdk").args(arg_set.split_whitespace()).run()
    };

    match cmd {
        Some(cmd) => {
            run(args[&cmd])?;
        }
        None => {
            for &arg_set in args.values() {
                run(arg_set)?;
            }
        }
    }

    Ok(())
}

fn run_appservice_tests() -> Result<()> {
    cmd!("rustup run stable cargo clippy -p matrix-sdk-appservice -- -D warnings").run()?;
    cmd!("rustup run stable cargo test -p matrix-sdk-appservice").run()?;

    Ok(())
}

fn run_wasm_checks(cmd: Option<WasmFeatureSet>) -> Result<()> {
    let args = BTreeMap::from([
        (WasmFeatureSet::MatrixQrcode, "-p matrix-qrcode"),
        (
            WasmFeatureSet::MatrixSdkNoDefault,
            "-p matrix-sdk \
             --no-default-features \
             --features qrcode,encryption,indexeddb_stores,rustls-tls",
        ),
        (WasmFeatureSet::MatrixSdkBase, "-p matrix-sdk-base"),
        (WasmFeatureSet::MatrixSdkCommon, "-p matrix-sdk-common"),
        (WasmFeatureSet::MatrixSdkCrypto, "-p matrix-sdk-crypto"),
        (
            WasmFeatureSet::MatrixSdkIndexeddbStores,
            "-p matrix-sdk --no-default-features --features indexeddb_stores,encryption,rustls-tls",
        ),
    ]);

    let run = |arg_set: &str| {
        cmd!("rustup run stable cargo clippy --target wasm32-unknown-unknown")
            .args(arg_set.split_whitespace())
            .args(["--", "-D", "warnings"])
            .run()
    };

    match cmd {
        Some(cmd) => {
            run(args[&cmd])?;
        }
        None => {
            for &arg_set in args.values() {
                run(arg_set)?;
            }
        }
    }

    Ok(())
}

fn workspace_root() -> Result<PathBuf> {
    #[derive(Deserialize)]
    struct Metadata {
        workspace_root: PathBuf,
    }

    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let metadata_json = cmd!("{cargo} metadata --no-deps --format-version 1").read()?;
    Ok(serde_json::from_str::<Metadata>(&metadata_json)?.workspace_root)
}
