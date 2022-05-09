use std::{env, path::PathBuf};

use clap::{Args, Subcommand};
use serde::Deserialize;
use xshell::{cmd, Shell};

use crate::Result;

#[derive(Args)]
pub struct FixupArgs {
    #[clap(subcommand)]
    cmd: Option<FixupCommand>,
}

#[derive(Subcommand)]
enum FixupCommand {
    /// Fix style
    Style,
    /// Fix typos
    Typos,
    /// Fix clippy lints
    Clippy,
}

impl FixupArgs {
    pub fn run(self) -> Result<()> {
        let sh = Shell::new()?;
        let _p = sh.push_dir(&workspace_root(&sh)?);

        match self.cmd {
            Some(cmd) => match cmd {
                FixupCommand::Style => fix_style(&sh),
                FixupCommand::Typos => fix_typos(&sh),
                FixupCommand::Clippy => fix_clippy(&sh),
            },
            None => {
                fix_style(&sh)?;
                fix_typos(&sh)?;
                fix_clippy(&sh)?;

                Ok(())
            }
        }
    }
}

fn fix_style(sh: &Shell) -> Result<()> {
    cmd!(sh, "rustup run nightly cargo fmt").run()?;
    Ok(())
}

fn fix_typos(sh: &Shell) -> Result<()> {
    // FIXME: Print install instructions if command-not-found (needs an xshell
    //        change: https://github.com/matklad/xshell/issues/46)
    cmd!(sh, "typos --write-changes").run()?;
    Ok(())
}

fn fix_clippy(sh: &Shell) -> Result<()> {
    cmd!(
        sh,
        "rustup run nightly cargo clippy --all-targets
        --fix --allow-dirty --allow-staged
        -- -D warnings "
    )
    .run()?;
    cmd!(
        sh,
        "rustup run nightly cargo clippy --workspace --all-targets
            --fix --allow-dirty --allow-staged
            --exclude matrix-sdk-crypto --exclude xtask
            --no-default-features --features native-tls,warp
            -- -D warnings"
    )
    .run()?;
    cmd!(
        sh,
        "rustup run nightly cargo clippy --all-targets -p matrix-sdk-crypto
            --allow-dirty --allow-staged --fix
            --no-default-features -- -D warnings"
    )
    .run()?;
    Ok(())
}

fn workspace_root(sh: &Shell) -> Result<PathBuf> {
    #[derive(Deserialize)]
    struct Metadata {
        workspace_root: PathBuf,
    }

    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let metadata_json = cmd!(sh, "{cargo} metadata --no-deps --format-version 1").read()?;
    Ok(serde_json::from_str::<Metadata>(&metadata_json)?.workspace_root)
}
