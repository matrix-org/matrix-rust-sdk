mod overview;
mod sync;

use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::{Result, sh, workspace};

#[derive(Args)]
pub struct LogArgs {
    #[clap(subcommand)]
    cmd: LogCommand,
}

/// Analysis around logs (formatted by `matrix-sdk-ffi`).
#[derive(Subcommand)]
enum LogCommand {
    /// Overview the logs as a tree where each node is a target and each leaf is
    /// a log location with occurrences, spans and fields.
    Overview {
        /// The file containing the logs to analyse.
        #[clap(long)]
        log_file: PathBuf,

        /// The output file that will receive the HTML report.
        #[clap(long)]
        html_output_file: PathBuf,
    },

    /// Visualise the sync requests and responses with a table and a duration
    /// graph.
    Sync {
        /// The file containing the logs to analyse.
        #[clap(long)]
        log_file: PathBuf,

        /// The output file that will receive the HTML report.
        #[clap(long)]
        html_output_file: PathBuf,
    },
}

impl LogArgs {
    pub fn run(self) -> Result<()> {
        let sh = sh();
        let _p = sh.push_dir(workspace::root_path()?);

        match self.cmd {
            LogCommand::Overview { log_file, html_output_file } => {
                overview::run(log_file, html_output_file)?
            }
            LogCommand::Sync { log_file, html_output_file } => {
                sync::run(log_file, html_output_file)?
            }
        }

        Ok(())
    }
}
