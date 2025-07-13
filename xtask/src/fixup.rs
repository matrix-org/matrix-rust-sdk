use clap::{Args, Subcommand};
use xshell::cmd;

use crate::{NIGHTLY, Result, sh, workspace};

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
        let sh = sh();
        let _p = sh.push_dir(workspace::root_path()?);

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
    let sh = sh();
    cmd!(sh, "rustup run {NIGHTLY} cargo fmt").run()?;
    Ok(())
}

fn fix_typos() -> Result<()> {
    let sh = sh();
    // FIXME: Print install instructions if command-not-found (needs an xshell
    //        change: https://github.com/matklad/xshell/issues/46)
    cmd!(sh, "typos --write-changes").run()?;
    Ok(())
}

fn fix_clippy() -> Result<()> {
    let sh = sh();
    cmd!(
        sh,
        "rustup run {NIGHTLY} cargo clippy --all-targets
        --fix --allow-dirty --allow-staged
        -- -D warnings "
    )
    .run()?;
    cmd!(
        sh,
        "rustup run {NIGHTLY} cargo clippy --workspace --all-targets
            --fix --allow-dirty --allow-staged
            --exclude matrix-sdk-crypto --exclude xtask
            --no-default-features --features native-tls,sso-login
            -- -D warnings"
    )
    .run()?;
    cmd!(
        sh,
        "rustup run {NIGHTLY} cargo clippy --all-targets -p matrix-sdk-crypto
            --allow-dirty --allow-staged --fix
            --no-default-features -- -D warnings"
    )
    .run()?;
    Ok(())
}
