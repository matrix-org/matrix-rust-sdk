use std::{env, path::PathBuf};

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
            },
            None => {
                check_style()?;
                check_clippy()?;
                check_typos()?;
                check_docs()?;

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

fn workspace_root() -> Result<PathBuf> {
    #[derive(Deserialize)]
    struct Metadata {
        workspace_root: PathBuf,
    }

    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let metadata_json = cmd!("{cargo} metadata --no-deps --format-version 1").read()?;
    Ok(serde_json::from_str::<Metadata>(&metadata_json)?.workspace_root)
}
