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
    NativeTls,
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
    /// Check `matrix-sdk` crate with `sqlite` feature (but not
    /// `e2e-encryption`) using in-memory vfs
    MatrixSdkSqliteStoresNoCryptoInMemory,
    /// Check `matrix-sdk` crate with `sqlite` and `e2e-encryption` features
    /// using in-memory vfs
    MatrixSdkSqliteStoresInMemory,
    /// Check `matrix-sdk` crate with `sqlite` feature (but not
    /// `e2e-encryption`) using OPFS vfs
    MatrixSdkSqliteStoresNoCryptoOpfs,
    /// Check `matrix-sdk` crate with `sqlite` and `e2e-encryption` features
    /// using OPFS vfs
    MatrixSdkSqliteStoresOpfs,
    /// Check `matrix-sdk` crate with `sqlite` feature (but not
    /// `e2e-encryption`) using IndexedDB vfs
    MatrixSdkSqliteStoresNoCryptoIndexeddb,
    /// Check `matrix-sdk` crate with `sqlite` and `e2e-encryption` features
    /// using IndexedDB vfs
    MatrixSdkSqliteStoresIndexeddb,
    /// Check `matrix-sdk-sqlite` crate with all features using in-memory vfs
    SqliteAllFeaturesInMemory,
    /// Check `matrix-sdk-sqlite` crate with all features using OPFS vfs
    SqliteAllFeaturesOpfs,
    /// Check `matrix-sdk-sqlite` crate with all features using IndexedDB vfs
    SqliteAllFeaturesIndexeddb,
    /// Check `matrix-sdk-sqlite` crate with `state-store` and
    /// `crypto-store` feature using in-memory vfs
    SqliteCryptoInMemory,
    /// Check `matrix-sdk-sqlite` crate with `state-store` and
    /// `crypto-store` feature using OPFS vfs
    SqliteCryptoOpfs,
    /// Check `matrix-sdk-sqlite` crate with `state-store` and
    /// `crypto-store` feature using IndexedDB vfs
    SqliteCryptoIndexeddb,
    /// Check `matrix-sdk-sqlite` crate with `state-store` feature using in-memory vfs
    SqliteStateInMemory,
    /// Check `matrix-sdk-sqlite` crate with `state-store` feature using OPFS vfs
    SqliteStateOpfs,
    /// Check `matrix-sdk-sqlite` crate with `state-store` feature using IndexedDB vfs
    SqliteStateIndexeddb,
    /// Check `matrix-sdk-sqlite` crate with `event-cache` feature using in-memory vfs
    SqliteCacheInMemory,
    /// Check `matrix-sdk-sqlite` crate with `event-cache` feature using OPFS vfs
    SqliteCacheOpfs,
    /// Check `matrix-sdk-sqlite` crate with `event-cache` feature using IndexedDB vfs
    SqliteCacheIndexeddb,
    /// Equivalent to `sqlite-all-features-in-memory`, `sqlite-crypto-in-memory`,
    /// `sqlite-state-in-memory`, and `sqlite-cache-in-memory`
    SqliteInMemory,
    /// Equivalent to `sqlite-all-features-opfs`, `sqlite-crypto-opfs`,
    /// `sqlite-state-opfs`, and `sqlite-cache-opfs`
    SqliteOpfs,
    /// Equivalent to `sqlite-all-features-indexeddb`, `sqlite-crypto-indexeddb`,
    /// `sqlite-state-indexeddb`, and `sqlite-cache-indexeddb`
    SqliteIndexeddb,
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
    cmd!(sh, "rustup run stable cargo build -p matrix-sdk-crypto-ffi -p matrix-sdk-ffi --features sentry,experimental-element-recent-emojis").run()?;
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
            --features rustls-tls,sso-login,sqlite,testing,experimental-element-recent-emojis
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
        (FeatureSet::NoEncryption, "--no-default-features --features sqlite,rustls-tls,testing"),
        (
            FeatureSet::NoSqlite,
            "--no-default-features --features e2e-encryption,rustls-tls,testing",
        ),
        (FeatureSet::NoEncryptionAndSqlite, "--no-default-features --features rustls-tls,testing"),
        (
            FeatureSet::SqliteCryptostore,
            "--no-default-features --features e2e-encryption,sqlite,rustls-tls,testing",
        ),
        (
            FeatureSet::ExperimentalEncryptedStateEvents,
            "--no-default-features --features experimental-encrypted-state-events,e2e-encryption,sqlite,rustls-tls,testing",
        ),
        (FeatureSet::NativeTls, "--no-default-features --features native-tls,testing"),
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

    if let Some(WasmFeatureSet::SqliteInMemory) = cmd {
        run_wasm_checks(Some(WasmFeatureSet::SqliteAllFeaturesInMemory))?;
        run_wasm_checks(Some(WasmFeatureSet::SqliteCryptoInMemory))?;
        run_wasm_checks(Some(WasmFeatureSet::SqliteStateInMemory))?;
        run_wasm_checks(Some(WasmFeatureSet::SqliteCacheInMemory))?;
        return Ok(());
    }

    if let Some(WasmFeatureSet::SqliteOpfs) = cmd {
        run_wasm_checks(Some(WasmFeatureSet::SqliteAllFeaturesOpfs))?;
        run_wasm_checks(Some(WasmFeatureSet::SqliteCryptoOpfs))?;
        run_wasm_checks(Some(WasmFeatureSet::SqliteStateOpfs))?;
        run_wasm_checks(Some(WasmFeatureSet::SqliteCacheOpfs))?;
        return Ok(());
    }

    if let Some(WasmFeatureSet::SqliteIndexeddb) = cmd {
        run_wasm_checks(Some(WasmFeatureSet::SqliteAllFeaturesIndexeddb))?;
        run_wasm_checks(Some(WasmFeatureSet::SqliteCryptoIndexeddb))?;
        run_wasm_checks(Some(WasmFeatureSet::SqliteStateIndexeddb))?;
        run_wasm_checks(Some(WasmFeatureSet::SqliteCacheIndexeddb))?;
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
        // In-memory vfs
        (
            WasmFeatureSet::MatrixSdkSqliteStoresNoCryptoInMemory,
            "-p matrix-sdk --no-default-features --features js,sqlite,bundled-sqlite,rustls-tls",
        ),
        (
            WasmFeatureSet::MatrixSdkSqliteStoresInMemory,
            "-p matrix-sdk --no-default-features --features js,sqlite,bundled-sqlite,e2e-encryption,rustls-tls",
        ),
        // OPFS vfs
        (
            WasmFeatureSet::MatrixSdkSqliteStoresNoCryptoOpfs,
            "-p matrix-sdk --no-default-features --features js,sqlite,bundled-sqlite,rustls-tls,vfs-opfs-sahpool",
        ),
        (
            WasmFeatureSet::MatrixSdkSqliteStoresOpfs,
            "-p matrix-sdk --no-default-features --features js,sqlite,bundled-sqlite,e2e-encryption,rustls-tls,vfs-opfs-sahpool",
        ),
        // IndexedDB vfs
        (
            WasmFeatureSet::MatrixSdkSqliteStoresNoCryptoIndexeddb,
            "-p matrix-sdk --no-default-features --features js,sqlite,bundled-sqlite,rustls-tls,vfs-relaxed-idb",
        ),
        (
            WasmFeatureSet::MatrixSdkSqliteStoresIndexeddb,
            "-p matrix-sdk --no-default-features --features js,sqlite,bundled-sqlite,e2e-encryption,rustls-tls,vfs-relaxed-idb",
        ),
        // In-memory vfs
        (
            WasmFeatureSet::SqliteAllFeaturesInMemory,
            "-p matrix-sdk-sqlite --no-default-features --features js,crypto-store,experimental-encrypted-state-events,state-store,event-cache",
        ),
        (
            WasmFeatureSet::SqliteCryptoInMemory,
            "-p matrix-sdk-sqlite --no-default-features --features js,state-store,crypto-store",
        ),
        (
            WasmFeatureSet::SqliteStateInMemory,
            "-p matrix-sdk-sqlite --no-default-features --features js,state-store",
        ),
        (
            WasmFeatureSet::SqliteCacheInMemory,
            "-p matrix-sdk-sqlite --no-default-features --features js,event-cache",
        ),
        // OPFS vfs
        (
            WasmFeatureSet::SqliteAllFeaturesOpfs,
            "-p matrix-sdk-sqlite --no-default-features --features js,vfs-opfs-sahpool,crypto-store,experimental-encrypted-state-events,state-store,event-cache",
        ),
        (
            WasmFeatureSet::SqliteCryptoOpfs,
            "-p matrix-sdk-sqlite --no-default-features --features js,vfs-opfs-sahpool,state-store,crypto-store",
        ),
        (
            WasmFeatureSet::SqliteStateOpfs,
            "-p matrix-sdk-sqlite --no-default-features --features js,vfs-opfs-sahpool,state-store",
        ),
        (
            WasmFeatureSet::SqliteCacheOpfs,
            "-p matrix-sdk-sqlite --no-default-features --features js,vfs-opfs-sahpool,event-cache",
        ),
        // IndexedDB vfs
        (
            WasmFeatureSet::SqliteAllFeaturesIndexeddb,
            "-p matrix-sdk-sqlite --no-default-features --features js,vfs-relaxed-idb,crypto-store,experimental-encrypted-state-events,state-store,event-cache",
        ),
        (
            WasmFeatureSet::SqliteCryptoIndexeddb,
            "-p matrix-sdk-sqlite --no-default-features --features js,vfs-relaxed-idb,state-store,crypto-store",
        ),
        (
            WasmFeatureSet::SqliteStateIndexeddb,
            "-p matrix-sdk-sqlite --no-default-features --features js,vfs-relaxed-idb,state-store",
        ),
        (
            WasmFeatureSet::SqliteCacheIndexeddb,
            "-p matrix-sdk-sqlite --no-default-features --features js,vfs-relaxed-idb,event-cache",
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

    if let Some(WasmFeatureSet::SqliteInMemory) = cmd {
        run_wasm_pack_tests(Some(WasmFeatureSet::SqliteAllFeaturesInMemory), runner)?;
        run_wasm_pack_tests(Some(WasmFeatureSet::SqliteCacheInMemory), runner)?;
        run_wasm_pack_tests(Some(WasmFeatureSet::SqliteStateInMemory), runner)?;
        run_wasm_pack_tests(Some(WasmFeatureSet::SqliteCryptoInMemory), runner)?;
        return Ok(());
    }

    if let Some(WasmFeatureSet::SqliteOpfs) = cmd {
        run_wasm_pack_tests(Some(WasmFeatureSet::SqliteAllFeaturesOpfs), runner)?;
        run_wasm_pack_tests(Some(WasmFeatureSet::SqliteCacheOpfs), runner)?;
        run_wasm_pack_tests(Some(WasmFeatureSet::SqliteStateOpfs), runner)?;
        run_wasm_pack_tests(Some(WasmFeatureSet::SqliteCryptoOpfs), runner)?;
        return Ok(());
    }

    if let Some(WasmFeatureSet::SqliteIndexeddb) = cmd {
        run_wasm_pack_tests(Some(WasmFeatureSet::SqliteAllFeaturesIndexeddb), runner)?;
        run_wasm_pack_tests(Some(WasmFeatureSet::SqliteCacheIndexeddb), runner)?;
        run_wasm_pack_tests(Some(WasmFeatureSet::SqliteStateIndexeddb), runner)?;
        run_wasm_pack_tests(Some(WasmFeatureSet::SqliteCryptoIndexeddb), runner)?;
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
            (
                "crates/matrix-sdk",
                "--no-default-features --features js,indexeddb,rustls-tls,testing --lib",
            ),
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
        // In-memory vfs
        (
            WasmFeatureSet::MatrixSdkSqliteStoresNoCryptoInMemory,
            (
                "crates/matrix-sdk",
                "--no-default-features --features js,bundled-sqlite,rustls-tls,testing --lib",
            ),
        ),
        (
            WasmFeatureSet::MatrixSdkSqliteStoresInMemory,
            (
                "crates/matrix-sdk",
                "--no-default-features --features js,bundled-sqlite,e2e-encryption,rustls-tls,testing --lib",
            ),
        ),
        // OPFS vfs
        //
        // OPFS vfs test suites has to be ran in release mode due to
        // a harmless debug assertion when closing database.
        //
        // Ref: https://github.com/Spxg/sqlite-wasm-rs/blob/master/crates/sqlite-wasm-vfs/src/sahpool.rs#L672
        (
            WasmFeatureSet::MatrixSdkSqliteStoresNoCryptoOpfs,
            (
                "crates/matrix-sdk",
                "--no-default-features --features js,bundled-sqlite,rustls-tls,testing,vfs-opfs-sahpool --lib --release",
            ),
        ),
        (
            WasmFeatureSet::MatrixSdkSqliteStoresOpfs,
            (
                "crates/matrix-sdk",
                "--no-default-features --features js,bundled-sqlite,e2e-encryption,rustls-tls,testing,vfs-opfs-sahpool --lib --release",
            ),
        ),
        // IndexedDB vfs
        (
            WasmFeatureSet::MatrixSdkSqliteStoresNoCryptoIndexeddb,
            (
                "crates/matrix-sdk",
                "--no-default-features --features js,bundled-sqlite,rustls-tls,testing,vfs-opfs-sahpool --lib",
            ),
        ),
        (
            WasmFeatureSet::MatrixSdkSqliteStoresIndexeddb,
            (
                "crates/matrix-sdk",
                "--no-default-features --features js,bundled-sqlite,e2e-encryption,rustls-tls,testing,vfs-opfs-sahpool --lib",
            ),
        ),
        // In-memory vfs
        (
            WasmFeatureSet::SqliteAllFeaturesInMemory,
            (
                "crates/matrix-sdk-sqlite",
                "--features js,state-store,experimental-encrypted-state-events,crypto-store,event-cache",
            ),
        ),
        (
            WasmFeatureSet::SqliteCryptoInMemory,
            (
                "crates/matrix-sdk-sqlite",
                "--no-default-features --features js,state-store,crypto-store",
            ),
        ),
        (
            WasmFeatureSet::SqliteStateInMemory,
            ("crates/matrix-sdk-sqlite", "--no-default-features --features js,state-store"),
        ),
        (
            WasmFeatureSet::SqliteCacheInMemory,
            ("crates/matrix-sdk-sqlite", "--no-default-features --features js,event-cache"),
        ),
        // OPFS vfs
        //
        // OPFS vfs test suites has to be ran in release mode due to
        // a harmless debug assertion when closing database.
        //
        // Ref: https://github.com/Spxg/sqlite-wasm-rs/blob/master/crates/sqlite-wasm-vfs/src/sahpool.rs#L672
        (
            WasmFeatureSet::SqliteAllFeaturesOpfs,
            (
                "crates/matrix-sdk-sqlite",
                "--features js,vfs-opfs-sahpool,state-store,experimental-encrypted-state-events,crypto-store,event-cache --release",
            ),
        ),
        (
            WasmFeatureSet::SqliteCryptoOpfs,
            (
                "crates/matrix-sdk-sqlite",
                "--no-default-features --features js,vfs-opfs-sahpool,state-store,crypto-store --release",
            ),
        ),
        (
            WasmFeatureSet::SqliteStateOpfs,
            (
                "crates/matrix-sdk-sqlite",
                "--no-default-features --features js,vfs-opfs-sahpool,state-store --release",
            ),
        ),
        (
            WasmFeatureSet::SqliteCacheOpfs,
            (
                "crates/matrix-sdk-sqlite",
                "--no-default-features --features js,vfs-opfs-sahpool,event-cache --release",
            ),
        ),
        // IndexedDB vfs
        (
            WasmFeatureSet::SqliteAllFeaturesIndexeddb,
            (
                "crates/matrix-sdk-sqlite",
                "--features js,vfs-relaxed-idb,state-store,experimental-encrypted-state-events,crypto-store,event-cache",
            ),
        ),
        (
            WasmFeatureSet::SqliteCryptoIndexeddb,
            (
                "crates/matrix-sdk-sqlite",
                "--no-default-features --features js,vfs-relaxed-idb,state-store,crypto-store",
            ),
        ),
        (
            WasmFeatureSet::SqliteStateIndexeddb,
            (
                "crates/matrix-sdk-sqlite",
                "--no-default-features --features js,vfs-relaxed-idb,state-store",
            ),
        ),
        (
            WasmFeatureSet::SqliteCacheIndexeddb,
            (
                "crates/matrix-sdk-sqlite",
                "--no-default-features --features js,vfs-relaxed-idb,event-cache",
            ),
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
