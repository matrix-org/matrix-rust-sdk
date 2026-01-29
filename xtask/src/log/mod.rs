use std::path::PathBuf;

use clap::{Args, Subcommand};
mod sync;

use crate::{Result, sh, workspace};

#[derive(Args)]
pub struct LogArgs {
    #[clap(subcommand)]
    cmd: LogCommand,
}

#[derive(Subcommand)]
enum LogCommand {
    /// Visualise the sync requests and responses.
    Sync {
        #[clap(long)]
        log_file: PathBuf,

        #[clap(long)]
        html_output_file: PathBuf,
    },
}

impl LogArgs {
    pub fn run(self) -> Result<()> {
        let sh = sh();
        let _p = sh.push_dir(workspace::root_path()?);

        match self.cmd {
            LogCommand::Sync { log_file, html_output_file } => {
                sync::run(log_file, html_output_file)?
            }
        }

        Ok(())
    }
}
