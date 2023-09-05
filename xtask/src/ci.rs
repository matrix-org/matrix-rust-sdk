use std::collections::BTreeMap;

use clap::{Args, Subcommand};
use xshell::{cmd, pushd};

use crate::{build_docs, workspace, DenyWarnings, Result};

const NIGHTLY: &str = "nightly-2023-07-03";
const WASM_TIMEOUT_ENV_KEY: &str = "WASM_BINDGEN_TEST_TIMEOUT";
const WASM_TIMEOUT_VALUE: &str = "120";

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

    /// Run tests with a specific feature set
    TestFeatures {
        #[clap(subcommand)]
        cmd: Option<FeatureSet>,
    },

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

    /// Check that bindings can be generated
    Bindings,

    /// Check that the examples compile
    Examples,
}

#[derive(Subcommand, PartialEq, Eq, PartialOrd, Ord)]
enum FeatureSet {
    NoEncryption,
    NoSqlite,
    NoEncryptionAndSqlite,
    SqliteCryptostore,
    RustlsTls,
    Markdown,
    Socks,
    SsoLogin,
}

#[derive(Subcommand, PartialEq, Eq, PartialOrd, Ord)]
#[allow(clippy::enum_variant_names)]
enum WasmFeatureSet {
    MatrixSdkQrcode,
    MatrixSdkNoDefault,
    MatrixSdkBase,
    MatrixSdkCommon,
    MatrixSdkIndexeddbStoresNoCrypto,
    MatrixSdkIndexeddbStores,
    IndexeddbNoCrypto,
    IndexeddbWithCrypto,
    Indexeddb,
}

impl CiArgs {
    pub fn run(self) -> Result<()> {
        let _p = pushd(workspace::root_path()?)?;

        match self.cmd {
            Some(cmd) => match cmd {
                CiCommand::Style => check_style(),
                CiCommand::Typos => check_typos(),
                CiCommand::Clippy => check_clippy(),
                CiCommand::Docs => check_docs(),
                CiCommand::TestFeatures { cmd } => run_feature_tests(cmd),
                CiCommand::Wasm { cmd } => run_wasm_checks(cmd),
                CiCommand::WasmPack { cmd } => run_wasm_pack_tests(cmd),
                CiCommand::TestCrypto => run_crypto_tests(),
                CiCommand::Bindings => check_bindings(),
                CiCommand::Examples => check_examples(),
            },
            None => {
                check_style()?;
                check_clippy()?;
                check_typos()?;
                check_docs()?;
                run_feature_tests(None)?;
                run_wasm_checks(None)?;
                run_crypto_tests()?;
                check_examples()?;

                Ok(())
            }
        }
    }
}

fn check_bindings() -> Result<()> {
    cmd!("rustup run stable cargo build -p matrix-sdk-crypto-ffi -p matrix-sdk-ffi").run()?;
    cmd!(
        "
        rustup run stable cargo run -p uniffi-bindgen -- generate
            --language kotlin
            --language swift
            --lib-file target/debug/libmatrix_sdk_ffi.a
            --out-dir target/generated-bindings
            bindings/matrix-sdk-ffi/src/api.udl
        "
    )
    .run()?;
    cmd!(
        "
        rustup run stable cargo run -p uniffi-bindgen -- generate
            --language kotlin
            --language swift
            --lib-file target/debug/libmatrix_sdk_crypto_ffi.a
            --out-dir target/generated-bindings
            bindings/matrix-sdk-crypto-ffi/src/olm.udl
        "
    )
    .run()?;

    Ok(())
}

fn check_examples() -> Result<()> {
    cmd!("rustup run stable cargo check -p example-*").run()?;
    Ok(())
}

fn check_style() -> Result<()> {
    cmd!("rustup run {NIGHTLY} cargo fmt -- --check").run()?;
    Ok(())
}

fn check_typos() -> Result<()> {
    // FIXME: Print install instructions if command-not-found (needs an xshell
    //        change: https://github.com/matklad/xshell/issues/46)
    cmd!("typos").run()?;
    Ok(())
}

fn check_clippy() -> Result<()> {
    cmd!("rustup run {NIGHTLY} cargo clippy --all-targets --features testing -- -D warnings")
        .run()?;
    cmd!(
        "rustup run {NIGHTLY} cargo clippy --workspace --all-targets
            --exclude matrix-sdk-crypto --exclude xtask
            --no-default-features
            --features native-tls,experimental-sliding-sync,sso-login,testing
            -- -D warnings"
    )
    .run()?;
    cmd!(
        "rustup run {NIGHTLY} cargo clippy --all-targets -p matrix-sdk-crypto
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
        (
            FeatureSet::NoEncryption,
            "--no-default-features --features sqlite,native-tls,experimental-sliding-sync,testing",
        ),
        (
            FeatureSet::NoSqlite,
            "--no-default-features --features e2e-encryption,native-tls,testing",
        ),
        (FeatureSet::NoEncryptionAndSqlite, "--no-default-features --features native-tls,testing"),
        (
            FeatureSet::SqliteCryptostore,
            "--no-default-features --features e2e-encryption,sqlite,native-tls,testing",
        ),
        (FeatureSet::RustlsTls, "--no-default-features --features rustls-tls,testing"),
        (FeatureSet::Markdown, "--features markdown,testing"),
        (FeatureSet::Socks, "--features socks,testing"),
        (FeatureSet::SsoLogin, "--features sso-login,testing"),
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
    cmd!("rustup run stable cargo nextest run -p matrix-sdk-crypto --no-default-features --features testing").run()?;
    cmd!("rustup run stable cargo nextest run -p matrix-sdk-crypto --features=backups_v1,testing")
        .run()?;
    cmd!("rustup run stable cargo test --doc -p matrix-sdk-crypto --features=backups_v1,testing")
        .run()?;
    cmd!(
        "rustup run stable cargo clippy -p matrix-sdk-crypto --features=experimental-algorithms -- -D warnings"
    )
    .run()?;
    cmd!(
        "rustup run stable cargo nextest run -p matrix-sdk-crypto --features=experimental-algorithms,testing"
    ).run()?;
    cmd!(
        "rustup run stable cargo test --doc -p matrix-sdk-crypto --features=experimental-algorithms,testing"
    )
    .run()?;

    cmd!("rustup run stable cargo nextest run -p matrix-sdk-crypto-ffi").run()?;

    cmd!(
        "rustup run stable cargo nextest run -p matrix-sdk-sqlite --features crypto-store,testing"
    )
    .run()?;

    Ok(())
}

fn run_wasm_checks(cmd: Option<WasmFeatureSet>) -> Result<()> {
    if let Some(WasmFeatureSet::Indexeddb) = cmd {
        run_wasm_checks(Some(WasmFeatureSet::IndexeddbNoCrypto))?;
        run_wasm_checks(Some(WasmFeatureSet::IndexeddbWithCrypto))?;
        return Ok(());
    }

    let args = BTreeMap::from([
        (WasmFeatureSet::MatrixSdkQrcode, "-p matrix-sdk-qrcode --features js"),
        (
            WasmFeatureSet::MatrixSdkNoDefault,
            "-p matrix-sdk --no-default-features --features js,rustls-tls",
        ),
        (WasmFeatureSet::MatrixSdkBase, "-p matrix-sdk-base --features js"),
        (WasmFeatureSet::MatrixSdkCommon, "-p matrix-sdk-common --features js"),
        (
            WasmFeatureSet::MatrixSdkIndexeddbStoresNoCrypto,
            "-p matrix-sdk --no-default-features --features js,indexeddb,rustls-tls",
        ),
        (
            WasmFeatureSet::MatrixSdkIndexeddbStores,
            "-p matrix-sdk --no-default-features --features js,indexeddb,e2e-encryption,rustls-tls",
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

fn run_wasm_pack_tests(cmd: Option<WasmFeatureSet>) -> Result<()> {
    if let Some(WasmFeatureSet::Indexeddb) = cmd {
        run_wasm_pack_tests(Some(WasmFeatureSet::IndexeddbNoCrypto))?;
        run_wasm_pack_tests(Some(WasmFeatureSet::IndexeddbWithCrypto))?;
        return Ok(());
    }
    let args = BTreeMap::from([
        (WasmFeatureSet::MatrixSdkQrcode, ("crates/matrix-sdk-qrcode", "--features js")),
        (
            WasmFeatureSet::MatrixSdkNoDefault,
            ("crates/matrix-sdk", "--no-default-features --features js,rustls-tls --lib"),
        ),
        (WasmFeatureSet::MatrixSdkBase, ("crates/matrix-sdk-base", "--features js")),
        (WasmFeatureSet::MatrixSdkCommon, ("crates/matrix-sdk-common", "--features js")),
        (
            WasmFeatureSet::MatrixSdkIndexeddbStoresNoCrypto,
            ("crates/matrix-sdk", "--no-default-features --features js,indexeddb,rustls-tls --lib"),
        ),
        (
            WasmFeatureSet::MatrixSdkIndexeddbStores,
            (
                "crates/matrix-sdk",
                "--no-default-features --features js,indexeddb,e2e-encryption,rustls-tls,testing --lib",
            ),
        ),
        (
            WasmFeatureSet::IndexeddbNoCrypto,
            ("crates/matrix-sdk-indexeddb", "--no-default-features"),
        ),
        (
            WasmFeatureSet::IndexeddbWithCrypto,
            ("crates/matrix-sdk-indexeddb", "--no-default-features --features e2e-encryption"),
        ),
    ]);

    let run = |(folder, arg_set): (&str, &str)| {
        let _pwd = pushd(folder)?;
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
