use std::path::PathBuf;

use matrix_sdk::SlidingSyncMode;
use structopt::{clap::arg_enum, StructOpt};

#[derive(Debug, StructOpt)]
#[structopt(name = "jack-in", about = "Your experimental sliding-sync jack into the matrix")]
pub struct Opt {
    /// The password of your account. If not given and no database found, it
    /// will prompt you for it
    #[structopt(short, long, env = "JACKIN_PASSWORD")]
    pub password: Option<String>,

    /// Create a fresh database, drop all existing cache
    #[structopt(long)]
    pub fresh: bool,

    /// RUST_LOG log-levels
    #[structopt(short, long, env = "JACKIN_LOG", default_value = "jack_in=info,warn")]
    pub log: String,

    /// The userID to log in with
    #[structopt(short, long, env = "JACKIN_USER")]
    pub user: String,

    /// The password to encrypt the store  with
    #[structopt(long, env = "JACKIN_STORE_PASSWORD")]
    pub store_pass: Option<String>,

    #[structopt(long)]
    /// Activate tracing and write the flamegraph to the specified file
    pub flames: Option<PathBuf>,

    #[structopt(flatten)]
    /// Sliding Sync configuration flags
    pub sliding_sync: SlidingSyncConfig,
}

arg_enum! {
    #[derive(Debug)]
    pub enum FullSyncMode {
        Growing,
        Paging,
    }
}

impl From<FullSyncMode> for SlidingSyncMode {
    fn from(val: FullSyncMode) -> Self {
        match val {
            FullSyncMode::Growing => SlidingSyncMode::GrowingFullSync,
            FullSyncMode::Paging => SlidingSyncMode::PagingFullSync,
        }
    }
}

#[derive(Debug, StructOpt)]
pub struct SlidingSyncConfig {
    /// The address of the sliding sync server to connect (probs the proxy)
    #[structopt(
        long = "sliding-sync-proxy",
        default_value = "http://localhost:8008",
        env = "JACKIN_SYNC_PROXY"
    )]
    pub proxy: String,

    /// Activate growing window rather than pagination for full-sync
    #[structopt(long, default_value = "SlidingSyncMode::Paging")]
    pub full_sync_mode: FullSyncMode,

    /// Limit the growing/paging to this number of maximum items to caonsider
    /// "done"
    #[structopt(long)]
    pub limit: Option<u32>,

    /// define the batch_size per request
    #[structopt(long)]
    pub batch_size: Option<u32>,

    /// define the timeline items to load
    #[structopt(long)]
    pub timeline_limit: Option<u32>,
}
