mod ci;
mod fixup;
mod kotlin;
mod swift;
mod workspace;

use ci::CiArgs;
use clap::{Parser, Subcommand};
use fixup::FixupArgs;
use kotlin::KotlinArgs;
use swift::SwiftArgs;
use xshell::cmd;

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

    // Keep in sync with .github/workflows/docs.yml
    cmd!("rustup run nightly cargo doc --no-deps --workspace --features docsrs")
        // Work around https://github.com/rust-lang/cargo/issues/10744
        .env("CARGO_TARGET_APPLIES_TO_HOST", "true")
        .env("RUSTDOCFLAGS", rustdocflags)
        .args(extra_args)
        .run()?;

    Ok(())
}
