use std::path::PathBuf;

use clap::{Args, Parser, ValueEnum};
use matrix_sdk::SlidingSyncMode;

#[derive(Debug, Parser)]
#[command(name = "jack-in", about = "Your experimental sliding-sync jack into the matrix")]
pub struct Opt {
    /// The password of your account. If not given and no database found, it
    /// will prompt you for it
    #[arg(short, long, env = "JACKIN_PASSWORD")]
    pub password: Option<String>,

    /// Create a fresh database, drop all existing cache
    #[arg(long)]
    pub fresh: bool,

    /// RUST_LOG log-levels
    #[arg(short, long, env = "JACKIN_LOG", default_value = "jack_in=info,warn")]
    pub log: String,

    /// The userID to log in with
    #[arg(short, long, env = "JACKIN_USER")]
    pub user: String,

    /// The password to encrypt the store  with
    #[arg(long, env = "JACKIN_STORE_PASSWORD")]
    pub store_pass: Option<String>,

    #[arg(long)]
    /// Activate tracing and write the flamegraph to the specified file
    pub flames: Option<PathBuf>,

    #[command(flatten)]
    /// Sliding Sync configuration flags
    pub sliding_sync: SlidingSyncConfig,
}

#[derive(Debug, Clone, ValueEnum)]
#[value(rename_all = "lower")]
pub enum FullSyncMode {
    Growing,
    Paging,
}

impl From<FullSyncMode> for SlidingSyncMode {
    fn from(val: FullSyncMode) -> Self {
        match val {
            FullSyncMode::Growing => SlidingSyncMode::Growing,
            FullSyncMode::Paging => SlidingSyncMode::Paging,
        }
    }
}

#[derive(Debug, Args)]
pub struct SlidingSyncConfig {
    /// The address of the sliding sync server to connect (probs the proxy)
    #[arg(
        long = "sliding-sync-proxy",
        default_value = "http://localhost:8008",
        env = "JACKIN_SYNC_PROXY"
    )]
    pub proxy: String,

    /// Activate growing window rather than pagination for full-sync
    #[arg(long, default_value = "paging")]
    pub full_sync_mode: FullSyncMode,

    /// Limit the growing/paging to this number of maximum items to caonsider
    /// "done"
    #[arg(long)]
    pub limit: Option<u32>,

    /// define the batch_size per request
    #[arg(long)]
    pub batch_size: Option<u32>,

    /// define the timeline items to load
    #[arg(long)]
    pub timeline_limit: Option<u32>,
}
