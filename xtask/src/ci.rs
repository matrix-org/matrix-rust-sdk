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

const WASM_TIMEOUT_ENV_KEY: &str = "WASM_BINDGEN_TEST_TIMEOUT";
const WASM_TIMEOUT_VALUE: &str = "120";

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
    /// Run wasm-pack tests
    WasmPack {
        #[clap(subcommand)]
        cmd: Option<WasmFeatureSet>,
    },
    /// Run tests for the different crypto crate features
    TestCrypto,
    /// Check that the examples compile
    Examples,
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
    ExperimentalTimeline,
}

#[derive(Subcommand, PartialEq, Eq, PartialOrd, Ord)]
#[allow(clippy::enum_variant_names)]
enum WasmFeatureSet {
    MatrixSdkQrcode,
    MatrixSdkNoDefault,
    MatrixSdkBase,
    MatrixSdkCommon,
    MatrixSdkCryptoJs,
    MatrixSdkIndexeddbStoresNoCrypto,
    MatrixSdkIndexeddbStores,
    IndexeddbNoCrypto,
    IndexeddbWithCrypto,
    Indexeddb,
    MatrixSdkCommandBot,
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
                CiCommand::TestFeatures { cmd } => run_feature_tests(cmd),
                CiCommand::TestAppservice => run_appservice_tests(),
                CiCommand::Wasm { cmd } => run_wasm_checks(cmd),
                CiCommand::WasmPack { cmd } => run_wasm_pack_tests(cmd),
                CiCommand::TestCrypto => run_crypto_tests(),
                CiCommand::Examples => check_examples(),
            },
            None => {
                check_style()?;
                check_clippy()?;
                check_typos()?;
                check_docs()?;
                run_feature_tests(None)?;
                run_appservice_tests()?;
                run_wasm_checks(None)?;
                run_crypto_tests()?;
                check_examples()?;

                Ok(())
            }
        }
    }
}

fn check_examples() -> Result<()> {
    cmd!("rustup run stable cargo check -p example-*").run()?;
    Ok(())
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
        "rustup run nightly cargo clippy --workspace --all-targets
            --exclude matrix-sdk-crypto --exclude xtask
            --no-default-features --features native-tls,warp
            -- -D warnings"
    )
    .run()?;
    cmd!(
        "rustup run nightly cargo clippy --all-targets -p matrix-sdk-crypto
            --no-default-features -- -D warnings"
    )
    .run()?;
    Ok(())
}

fn check_docs() -> Result<()> {
    build_docs([], DenyWarnings::Yes)
}

fn run_feature_tests(cmd: Option<FeatureSet>) -> Result<()> {
    let args = BTreeMap::from([
        (FeatureSet::NoEncryption, "--no-default-features --features sled,native-tls"),
        (FeatureSet::NoSled, "--no-default-features --features e2e-encryption,native-tls"),
        (FeatureSet::NoEncryptionAndSled, "--no-default-features --features native-tls"),
        (
            FeatureSet::SledCryptostore,
            "--no-default-features --features e2e-encryption,sled,native-tls",
        ),
        (FeatureSet::RustlsTls, "--no-default-features --features rustls-tls"),
        (FeatureSet::Markdown, "--features markdown"),
        (FeatureSet::Socks, "--features socks"),
        (FeatureSet::SsoLogin, "--features sso-login"),
        (FeatureSet::ExperimentalTimeline, "--features experimental-timeline"),
    ]);

    let run = |arg_set: &str| {
        cmd!("rustup run stable cargo nextest run -p matrix-sdk")
            .args(arg_set.split_whitespace())
            .run()?;
        cmd!("rustup run stable cargo test --doc -p matrix-sdk")
            .args(arg_set.split_whitespace())
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

fn run_crypto_tests() -> Result<()> {
    cmd!(
        "rustup run stable cargo clippy -p matrix-sdk-crypto --features=backups_v1 -- -D warnings"
    )
    .run()?;
    cmd!("rustup run stable cargo nextest run -p matrix-sdk-crypto --features=backups_v1").run()?;
    cmd!("rustup run stable cargo test --doc -p matrix-sdk-crypto --features=backups_v1").run()?;
    cmd!(
        "rustup run stable cargo clippy -p matrix-sdk-crypto --features=experimental-algorithms -- -D warnings"
    )
    .run()?;
    cmd!(
        "rustup run stable cargo nextest run -p matrix-sdk-crypto --features=experimental-algorithms"
    ).run()?;
    cmd!(
        "rustup run stable cargo test --doc -p matrix-sdk-crypto --features=experimental-algorithms"
    )
    .run()?;

    cmd!("rustup run stable cargo nextest run -p matrix-sdk-crypto-ffi").run()?;

    Ok(())
}

fn run_appservice_tests() -> Result<()> {
    cmd!("rustup run stable cargo clippy -p matrix-sdk-appservice -- -D warnings").run()?;
    cmd!("rustup run stable cargo nextest run -p matrix-sdk-appservice").run()?;
    cmd!("rustup run stable cargo test --doc -p matrix-sdk-appservice").run()?;

    Ok(())
}

fn run_wasm_checks(cmd: Option<WasmFeatureSet>) -> Result<()> {
    if let Some(WasmFeatureSet::Indexeddb) = cmd {
        run_wasm_checks(Some(WasmFeatureSet::IndexeddbNoCrypto))?;
        run_wasm_checks(Some(WasmFeatureSet::IndexeddbWithCrypto))?;
        return Ok(());
    }

    let args = BTreeMap::from([
        (WasmFeatureSet::MatrixSdkQrcode, "-p matrix-sdk-qrcode"),
        (
            WasmFeatureSet::MatrixSdkNoDefault,
            "-p matrix-sdk --no-default-features --features rustls-tls",
        ),
        (WasmFeatureSet::MatrixSdkBase, "-p matrix-sdk-base"),
        (WasmFeatureSet::MatrixSdkCommon, "-p matrix-sdk-common"),
        (WasmFeatureSet::MatrixSdkCryptoJs, "-p matrix-sdk-crypto-js"),
        (
            WasmFeatureSet::MatrixSdkIndexeddbStoresNoCrypto,
            "-p matrix-sdk --no-default-features --features indexeddb,rustls-tls",
        ),
        (
            WasmFeatureSet::MatrixSdkIndexeddbStores,
            "-p matrix-sdk --no-default-features --features indexeddb,e2e-encryption,rustls-tls",
        ),
        (WasmFeatureSet::IndexeddbNoCrypto, "-p matrix-sdk-indexeddb --no-default-features "),
        (
            WasmFeatureSet::IndexeddbWithCrypto,
            "-p matrix-sdk-indexeddb --no-default-features --features e2e-encryption",
        ),
    ]);

    let run = |arg_set: &str| {
        cmd!("rustup run stable cargo clippy --target wasm32-unknown-unknown")
            .args(arg_set.split_whitespace())
            .args(["--", "-D", "warnings"])
            .env(WASM_TIMEOUT_ENV_KEY, WASM_TIMEOUT_VALUE)
            .run()
    };

    let test_command_bot = || {
        let _p = pushd("examples/wasm_command_bot");

        cmd!("rustup run stable cargo clippy --target wasm32-unknown-unknown")
            .args(["--", "-D", "warnings", "-A", "clippy::unused-unit"])
            .env(WASM_TIMEOUT_ENV_KEY, WASM_TIMEOUT_VALUE)
            .run()
    };

    match cmd {
        Some(cmd) => match cmd {
            WasmFeatureSet::MatrixSdkCommandBot => {
                test_command_bot()?;
            }
            _ => {
                run(args[&cmd])?;
            }
        },
        None => {
            for &arg_set in args.values() {
                run(arg_set)?;
            }

            test_command_bot()?;
        }
    }

    Ok(())
}

fn run_wasm_pack_tests(cmd: Option<WasmFeatureSet>) -> Result<()> {
    if let Some(WasmFeatureSet::Indexeddb) = cmd {
        run_wasm_pack_tests(Some(WasmFeatureSet::IndexeddbNoCrypto))?;
        run_wasm_pack_tests(Some(WasmFeatureSet::IndexeddbWithCrypto))?;
        return Ok(());
    }
    let args = BTreeMap::from([
        (WasmFeatureSet::MatrixSdkQrcode, ("matrix-sdk-qrcode", "")),
        (
            WasmFeatureSet::MatrixSdkNoDefault,
            ("matrix-sdk", "--no-default-features --features rustls-tls --lib"),
        ),
        (WasmFeatureSet::MatrixSdkBase, ("matrix-sdk-base", "")),
        (WasmFeatureSet::MatrixSdkCommon, ("matrix-sdk-common", "")),
        (WasmFeatureSet::MatrixSdkCryptoJs, ("matrix-sdk-crypto-js", "")),
        (
            WasmFeatureSet::MatrixSdkIndexeddbStoresNoCrypto,
            ("matrix-sdk", "--no-default-features --features indexeddb,rustls-tls --lib"),
        ),
        (
            WasmFeatureSet::MatrixSdkIndexeddbStores,
            (
                "matrix-sdk",
                "--no-default-features --features indexeddb,e2e-encryption,rustls-tls --lib",
            ),
        ),
        (WasmFeatureSet::IndexeddbNoCrypto, ("matrix-sdk-indexeddb", "--no-default-features")),
        (
            WasmFeatureSet::IndexeddbWithCrypto,
            ("matrix-sdk-indexeddb", "--no-default-features --features e2e-encryption"),
        ),
    ]);

    let run = |(folder, arg_set): (&str, &str)| {
        let _p = pushd(format!("crates/{folder}"));
        cmd!("pwd").env(WASM_TIMEOUT_ENV_KEY, WASM_TIMEOUT_VALUE).run()?; // print dir so we know what might have failed
        cmd!("wasm-pack test --node -- ")
            .args(arg_set.split_whitespace())
            .env(WASM_TIMEOUT_ENV_KEY, WASM_TIMEOUT_VALUE)
            .run()?;
        cmd!("wasm-pack test --firefox --headless --")
            .args(arg_set.split_whitespace())
            .env(WASM_TIMEOUT_ENV_KEY, WASM_TIMEOUT_VALUE)
            .run()
    };

    let test_command_bot = || {
        let _p = pushd("examples/wasm_command_bot");
        cmd!("wasm-pack test --node").env(WASM_TIMEOUT_ENV_KEY, WASM_TIMEOUT_VALUE).run()?;
        cmd!("wasm-pack test --firefox --headless")
            .env(WASM_TIMEOUT_ENV_KEY, WASM_TIMEOUT_VALUE)
            .run()
    };

    match cmd {
        Some(cmd) => match cmd {
            WasmFeatureSet::MatrixSdkCommandBot => {
                test_command_bot()?;
            }
            _ => {
                run(args[&cmd])?;
            }
        },
        None => {
            for &arg_set in args.values() {
                run(arg_set)?;
            }

            test_command_bot()?;
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
