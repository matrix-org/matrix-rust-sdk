use std::{env, path::PathBuf};

use clap::{Args, Subcommand};
use serde::Deserialize;
use xshell::{cmd, pushd};

use crate::Result;

#[derive(Args)]
pub struct FixupArgs {
    #[clap(subcommand)]
    cmd: Option<FixupCommand>,
}

#[derive(Subcommand)]
enum FixupCommand {
    /// Check style
    Style,
    /// Check for typos
    Typos,
    /// Check clippy lints
    Clippy,
}

impl FixupArgs {
    pub fn run(self) -> Result<()> {
        let _p = pushd(&workspace_root()?)?;

        match self.cmd {
            Some(cmd) => match cmd {
                FixupCommand::Style => fix_style(),
                FixupCommand::Typos => fix_typos(),
                FixupCommand::Clippy => fix_clippy(),
            },
            None => {
                fix_style()?;
                fix_typos()?;
                fix_clippy()?;

                Ok(())
            }
        }
    }
}

fn fix_style() -> Result<()> {
    cmd!("rustup run nightly cargo fmt").run()?;
    Ok(())
}

fn fix_typos() -> Result<()> {
    // FIXME: Print install instructions if command-not-found (needs an xshell
    //        change: https://github.com/matklad/xshell/issues/46)
    cmd!("typos --write-changes").run()?;
    Ok(())
}

fn fix_clippy() -> Result<()> {
    cmd!(
        "rustup run nightly cargo clippy --all-targets
        --fix --allow-dirty --allow-staged
        -- -D warnings "
    )
    .run()?;
    cmd!(
        "rustup run nightly cargo clippy --workspace --all-targets
            --fix --allow-dirty --allow-staged
            --exclude matrix-sdk-crypto --exclude xtask
            --no-default-features --features native-tls,warp
            -- -D warnings"
    )
    .run()?;
    cmd!(
        "rustup run nightly cargo clippy --all-targets -p matrix-sdk-crypto
            --allow-dirty --allow-staged --fix
            --no-default-features -- -D warnings"
    )
    .run()?;
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
