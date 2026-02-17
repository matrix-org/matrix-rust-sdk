use std::{
    collections::BTreeMap,
    env::consts::{DLL_PREFIX, DLL_SUFFIX},
    fmt::Display,
};

use clap::{Args, Subcommand, ValueEnum};
use xshell::cmd;

use crate::{DenyWarnings, NIGHTLY, Result, build_docs, sh, workspace};

const WASM_TIMEOUT_ENV_KEY: &str = "WASM_BINDGEN_TEST_TIMEOUT";
const WASM_TIMEOUT_VALUE: &str = "180";

#[derive(Args)]
pub struct CiArgs {
    #[clap(subcommand)]
    cmd: Option<CiCommand>,
}

/// The kind of runner for WebAssembly tests run with `wasm-pack test`.
#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum WasmTestRunner {
    // Run with all available runners.
    #[default]
    All,
    // Run with the Node.js runner.
    Node,
    // Run with the Firefox runner.
    Firefox,
    // Run with the Chrome runner.
    Chrome,
}

impl Display for WasmTestRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WasmTestRunner::All => write!(f, "all"),
            WasmTestRunner::Node => write!(f, "node"),
            WasmTestRunner::Firefox => write!(f, "firefox"),
            WasmTestRunner::Chrome => write!(f, "chrome"),
        }
    }
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

    /// Run clippy checks for the wasm target
    Wasm {
        #[clap(subcommand)]
        cmd: Option<WasmFeatureSet>,
    },

    /// Run tests with `wasm-pack test`
    WasmPack {
        #[clap(subcommand)]
        cmd: Option<WasmFeatureSet>,

        #[clap(long, default_value_t = WasmTestRunner::All)]
        runner: WasmTestRunner,
    },

    /// Run tests for the different crypto crate features
    TestCrypto,

    /// Check that bindings can be generated
    Bindings,

    /// Check that the examples compile
    Examples,

    /// Run the workspace tests and create a code coverage report using
    /// llvm-cov.
    ///
    /// Note: This requires the docker container for the integration tests to be
    /// running.
    Coverage {
        /// Specify the output format that we're going to use.
        #[arg(long, short, default_value_t = CoverageOutputFormat::Text)]
        output_format: CoverageOutputFormat,
    },
}

#[derive(Clone, Debug, Default, ValueEnum)]
enum CoverageOutputFormat {
    /// Output the coverage report to stdout.
    #[default]
    Text,
    /// Output the coverage report as a HTML report in the target/llvm-cov/html
    /// folder.
    Html,
    /// Output the coverage report as the custom Codecov coverage format.
    /// folder.
    Codecov,
}

impl Display for CoverageOutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoverageOutputFormat::Text => write!(f, "text"),
            CoverageOutputFormat::Html => write!(f, "html"),
            CoverageOutputFormat::Codecov => write!(f, "codecov"),
        }
    }
}

#[derive(Subcommand, PartialEq, Eq, PartialOrd, Ord)]
enum FeatureSet {
    NoEncryption,
    NoSqlite,
    NoEncryptionAndSqlite,
    SqliteCryptostore,
    ExperimentalEncryptedStateEvents,
    RustlsTls,
    Markdown,
    Socks,
    SsoLogin,
    Search,
    ElementRecentEmojis,
}

#[derive(Subcommand, PartialEq, Eq, PartialOrd, Ord)]
#[allow(clippy::enum_variant_names)]
enum WasmFeatureSet {
    /// Check `matrix-sdk-qrcode` crate
    MatrixSdkQrcode,
    /// Check `matrix-sdk-base` crate
    MatrixSdkBase,
    /// Check `matrix-sdk-common` crate
    MatrixSdkCommon,
    /// Check `matrix-sdk` crate with no default features
    MatrixSdkNoDefault,
    /// Check `matrix-sdk-ui` crate
    MatrixSdkUi,
    /// Check `matrix-sdk` crate with `indexeddb` feature (but not
    /// `e2e-encryption`)
    MatrixSdkIndexeddbStoresNoCrypto,
    /// Check `matrix-sdk` crate with `indexeddb` and `e2e-encryption` features
    MatrixSdkIndexeddbStores,
    /// Check `matrix-sdk-indexeddb` crate with all features
    IndexeddbAllFeatures,
    /// Check `matrix-sdk-indexeddb` crate with `e2e-encryption` feature
    IndexeddbCrypto,
    /// Check `matrix-sdk-indexeddb` crate with `state-store` feature
    IndexeddbState,
    /// Equivalent to `indexeddb-all-features`, `indexeddb-crypto` and
    /// `indexeddb-state`
    Indexeddb,
}

impl CiArgs {
    pub fn run(self) -> Result<()> {
        let sh = sh();
        let _p = sh.push_dir(workspace::root_path()?);

        match self.cmd {
            Some(cmd) => match cmd {
                CiCommand::Style => check_style(),
                CiCommand::Typos => check_typos(),
                CiCommand::Clippy => check_clippy(),
                CiCommand::Docs => check_docs(),
                CiCommand::TestFeatures { cmd } => run_feature_tests(cmd),
                CiCommand::Wasm { cmd } => run_wasm_checks(cmd),
                CiCommand::WasmPack { cmd, runner } => run_wasm_pack_tests(cmd, runner),
                CiCommand::TestCrypto => run_crypto_tests(),
                CiCommand::Bindings => check_bindings(),
                CiCommand::Examples => check_examples(),
                CiCommand::Coverage { output_format } => run_coverage(output_format),
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
    let sh = sh();
    cmd!(sh, "rustup run stable cargo build -p matrix-sdk-crypto-ffi -p matrix-sdk-ffi --features native-tls,sentry,experimental-element-recent-emojis").run()?;
    cmd!(
        sh,
        "
        rustup run stable cargo run -p uniffi-bindgen -- generate
            --library
            --language kotlin
            --language swift
            --out-dir target/generated-bindings
            target/debug/{DLL_PREFIX}matrix_sdk_ffi{DLL_SUFFIX}
        "
    )
    .run()?;
    cmd!(
        sh,
        "
        rustup run stable cargo run -p uniffi-bindgen -- generate
            --library
            --language kotlin
            --language swift
            --out-dir target/generated-bindings
            target/debug/{DLL_PREFIX}matrix_sdk_crypto_ffi{DLL_SUFFIX}
        "
    )
    .run()?;

    Ok(())
}

fn check_examples() -> Result<()> {
    let sh = sh();
    cmd!(sh, "rustup run stable cargo check -p example-*").run()?;
    Ok(())
}

fn check_style() -> Result<()> {
    let sh = sh();
    cmd!(sh, "rustup run {NIGHTLY} cargo fmt -- --check").run()?;
    Ok(())
}

fn check_typos() -> Result<()> {
    let sh = sh();
    // FIXME: Print install instructions if command-not-found (needs an xshell
    //        change: https://github.com/matklad/xshell/issues/46)
    cmd!(sh, "typos").run()?;
    Ok(())
}

fn check_clippy() -> Result<()> {
    let sh = sh();
    cmd!(sh, "rustup run {NIGHTLY} cargo clippy --all-targets --features testing -- -D warnings")
        .run()?;

    cmd!(
        sh,
        "rustup run {NIGHTLY} cargo clippy --workspace --all-targets
            --exclude matrix-sdk-crypto --exclude xtask
            --no-default-features
            --features native-tls,sso-login,sqlite,testing,experimental-element-recent-emojis
            -- -D warnings"
    )
    .run()?;

    cmd!(
        sh,
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
        (FeatureSet::NoEncryption, "--no-default-features --features sqlite,native-tls,testing"),
        (
            FeatureSet::NoSqlite,
            "--no-default-features --features e2e-encryption,native-tls,testing",
        ),
        (FeatureSet::NoEncryptionAndSqlite, "--no-default-features --features native-tls,testing"),
        (
            FeatureSet::SqliteCryptostore,
            "--no-default-features --features e2e-encryption,sqlite,native-tls,testing",
        ),
        (
            FeatureSet::ExperimentalEncryptedStateEvents,
            "--no-default-features --features experimental-encrypted-state-events,e2e-encryption,sqlite,native-tls,testing",
        ),
        (FeatureSet::RustlsTls, "--no-default-features --features rustls-tls,testing"),
        (FeatureSet::Markdown, "--features markdown,testing"),
        (FeatureSet::Socks, "--features socks,testing"),
        (FeatureSet::SsoLogin, "--features sso-login,testing"),
        (FeatureSet::Search, "--features experimental-search"),
        (FeatureSet::ElementRecentEmojis, "--features experimental-element-recent-emojis"),
    ]);

    let sh = sh();
    let run = |arg_set: &str| {
        cmd!(sh, "rustup run stable cargo nextest run -p matrix-sdk")
            .args(arg_set.split_whitespace())
            .run()?;
        cmd!(sh, "rustup run stable cargo test --doc -p matrix-sdk")
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
    let sh = sh();
    cmd!(sh, "rustup run stable cargo clippy -p matrix-sdk-crypto -- -D warnings").run()?;
    cmd!(sh, "rustup run stable cargo nextest run -p matrix-sdk-crypto --no-default-features --features testing").run()?;
    cmd!(sh, "rustup run stable cargo nextest run -p matrix-sdk-crypto --features=testing")
        .run()?;
    cmd!(sh, "rustup run stable cargo test --doc -p matrix-sdk-crypto --features=testing").run()?;
    cmd!(
        sh,
        "rustup run stable cargo clippy -p matrix-sdk-crypto --features=experimental-algorithms -- -D warnings"
    )
    .run()?;
    cmd!(
        sh,
        "rustup run stable cargo nextest run -p matrix-sdk-crypto --features=experimental-algorithms,testing"
    ).run()?;
    cmd!(
        sh,
        "rustup run stable cargo test --doc -p matrix-sdk-crypto --features=experimental-algorithms,testing"
    )
    .run()?;
    cmd!(sh, "rustup run stable cargo nextest run -p matrix-sdk-crypto --features=experimental-encrypted-state-events").run()?;

    cmd!(sh, "rustup run stable cargo nextest run -p matrix-sdk-crypto-ffi").run()?;

    cmd!(
        sh,
        "rustup run stable cargo nextest run -p matrix-sdk-sqlite --features crypto-store,testing"
    )
    .run()?;

    Ok(())
}

fn run_wasm_checks(cmd: Option<WasmFeatureSet>) -> Result<()> {
    if let Some(WasmFeatureSet::Indexeddb) = cmd {
        run_wasm_checks(Some(WasmFeatureSet::IndexeddbAllFeatures))?;
        run_wasm_checks(Some(WasmFeatureSet::IndexeddbCrypto))?;
        run_wasm_checks(Some(WasmFeatureSet::IndexeddbState))?;
        return Ok(());
    }

    let args = BTreeMap::from([
        (WasmFeatureSet::MatrixSdkQrcode, "-p matrix-sdk-qrcode --features js"),
        (
            WasmFeatureSet::MatrixSdkNoDefault,
            "-p matrix-sdk --no-default-features --features js,rustls-tls",
        ),
        (WasmFeatureSet::MatrixSdkBase, "-p matrix-sdk-base --features js,test-send-sync"),
        (WasmFeatureSet::MatrixSdkCommon, "-p matrix-sdk-common --features js"),
        (WasmFeatureSet::MatrixSdkUi, "-p matrix-sdk-ui --features js"),
        (
            WasmFeatureSet::MatrixSdkIndexeddbStoresNoCrypto,
            "-p matrix-sdk --no-default-features --features js,indexeddb,rustls-tls",
        ),
        (
            WasmFeatureSet::MatrixSdkIndexeddbStores,
            "-p matrix-sdk --no-default-features --features js,indexeddb,e2e-encryption,rustls-tls",
        ),
        (WasmFeatureSet::IndexeddbAllFeatures, "-p matrix-sdk-indexeddb"),
        (
            WasmFeatureSet::IndexeddbCrypto,
            "-p matrix-sdk-indexeddb --no-default-features --features e2e-encryption",
        ),
        (
            WasmFeatureSet::IndexeddbState,
            "-p matrix-sdk-indexeddb --no-default-features --features state-store",
        ),
    ]);

    let sh = sh();
    let run = |arg_set: &str| {
        cmd!(sh, "rustup run stable cargo clippy --target wasm32-unknown-unknown")
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

fn run_wasm_pack_tests(cmd: Option<WasmFeatureSet>, runner: WasmTestRunner) -> Result<()> {
    if let Some(WasmFeatureSet::Indexeddb) = cmd {
        run_wasm_pack_tests(Some(WasmFeatureSet::IndexeddbAllFeatures), runner)?;
        run_wasm_pack_tests(Some(WasmFeatureSet::IndexeddbCrypto), runner)?;
        run_wasm_pack_tests(Some(WasmFeatureSet::IndexeddbState), runner)?;
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
        (WasmFeatureSet::IndexeddbAllFeatures, ("crates/matrix-sdk-indexeddb", "")),
        (
            WasmFeatureSet::IndexeddbCrypto,
            ("crates/matrix-sdk-indexeddb", "--no-default-features --features e2e-encryption"),
        ),
        (
            WasmFeatureSet::IndexeddbState,
            ("crates/matrix-sdk-indexeddb", "--no-default-features --features state-store"),
        ),
    ]);

    let sh = sh();
    let run = |runner: WasmTestRunner, (folder, arg_set): (&str, &str)| {
        let _pwd = sh.push_dir(folder);

        cmd!(sh, "pwd").run()?; // print dir so we know what might have failed

        if matches!(runner, WasmTestRunner::All | WasmTestRunner::Node) {
            cmd!(sh, "wasm-pack test --node -- ")
                .args(arg_set.split_whitespace())
                .env(WASM_TIMEOUT_ENV_KEY, WASM_TIMEOUT_VALUE)
                .run()?;
        }

        if matches!(runner, WasmTestRunner::All | WasmTestRunner::Firefox) {
            cmd!(sh, "wasm-pack test --firefox --headless --")
                .args(arg_set.split_whitespace())
                .env(WASM_TIMEOUT_ENV_KEY, WASM_TIMEOUT_VALUE)
                .run()?;
        }

        if matches!(runner, WasmTestRunner::All | WasmTestRunner::Chrome) {
            cmd!(sh, "wasm-pack test --chrome --headless --")
                .args(arg_set.split_whitespace())
                .env(WASM_TIMEOUT_ENV_KEY, WASM_TIMEOUT_VALUE)
                .run()?;
        }

        Ok::<_, xshell::Error>(())
    };

    match cmd {
        Some(cmd) => {
            run(runner, args[&cmd])?;
        }
        None => {
            for &arg_set in args.values() {
                run(runner, arg_set)?;
            }
        }
    }

    Ok(())
}

fn run_coverage(output_format: CoverageOutputFormat) -> Result<()> {
    let sh = sh();
    let cmd = cmd!(sh, "rustup run stable cargo llvm-cov nextest");
    let cmd = cmd.args([
        "--workspace",
        "--exclude",
        "matrix-sdk-indexeddb",
        "--ignore-filename-regex",
        "testing/*|bindings/*|uniffi-bindgen|labs/*",
    ]);

    let cmd = match output_format {
        CoverageOutputFormat::Text => cmd,
        CoverageOutputFormat::Html => cmd.arg("--html"),
        CoverageOutputFormat::Codecov => {
            cmd.args(["--codecov", "--output-path", "coverage.xml", "--profile", "ci"])
        }
    };

    cmd.run()?;

    Ok(())
}
