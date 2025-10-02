#![allow(unexpected_cfgs)]

mod ci;
mod fixup;
mod kotlin;
mod release;
mod swift;
mod workspace;

use std::rc::Rc;

use ci::CiArgs;
use clap::{Parser, Subcommand};
use fixup::FixupArgs;
use kotlin::KotlinArgs;
use release::ReleaseArgs;
use swift::SwiftArgs;
use xshell::{Shell, cmd};

const NIGHTLY: &str = "nightly-2025-10-01";

type Result<T, E = Box<dyn std::error::Error>> = std::result::Result<T, E>;

#[derive(Parser)]
struct Xtask {
    #[clap(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run continuous integration checks
    Ci(CiArgs),
    /// Fix up automatic checks
    Fixup(FixupArgs),
    /// Build the SDKs documentation
    Doc {
        /// Opens the docs in a browser after the operation
        #[clap(long)]
        open: bool,
    },
    /// Prepare and publish a release of the matrix-sdk crates
    Release(ReleaseArgs),
    Swift(SwiftArgs),
    Kotlin(KotlinArgs),
}

fn main() -> Result<()> {
    match Xtask::parse().cmd {
        Command::Ci(ci) => ci.run(),
        Command::Fixup(cfg) => cfg.run(),
        Command::Doc { open } => build_docs(open.then_some("--open"), DenyWarnings::No),
        Command::Swift(cfg) => cfg.run(),
        Command::Kotlin(cfg) => cfg.run(),
        Command::Release(cfg) => cfg.run(),
    }
}

enum DenyWarnings {
    Yes,
    No,
}

fn build_docs(
    extra_args: impl IntoIterator<Item = &'static str>,
    deny_warnings: DenyWarnings,
) -> Result<()> {
    let mut rustdocflags = "--enable-index-page -Zunstable-options --cfg docsrs".to_owned();
    if let DenyWarnings::Yes = deny_warnings {
        rustdocflags += " -Dwarnings";
    }

    let sh = sh();
    // Keep in sync with .github/workflows/docs.yml
    cmd!(sh, "rustup run {NIGHTLY} cargo doc --no-deps --workspace --features docsrs")
        .env("RUSTDOCFLAGS", rustdocflags)
        .args(extra_args)
        .run()?;

    Ok(())
}

thread_local! {
    /// The shared shell API.
    static SH: Rc<Shell> = Rc::new(Shell::new().unwrap())
}

/// Get a reference to the shared shell API.
fn sh() -> Rc<Shell> {
    SH.with(|sh| sh.clone())
}
