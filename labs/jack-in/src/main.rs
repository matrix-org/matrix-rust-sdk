//! ## Demo
//!
//! `Demo` shows how to use tui-realm in a real case

use std::path::{Path, PathBuf};

use eyre::{eyre, Result};
use matrix_sdk::{
    ruma::{OwnedDeviceId, OwnedRoomId, OwnedUserId},
    Client, Session,
};
use tracing::{log::LevelFilter, warn};
use tracing_flame::FlameLayer;
use tracing_subscriber::prelude::*;
use tuirealm::{application::PollStrategy, Event, Update};

// -- internal
mod app;
mod client;
mod components;
use app::model::Model;
use tokio::sync::mpsc;

// Let's define the messages handled by our app. NOTE: it must derive
// `PartialEq`
#[derive(PartialEq, Eq)]
pub enum Msg {
    AppClose,
    Clock,
    RoomsBlur,
    DetailsBlur,
    SelectRoom(Option<OwnedRoomId>),
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum JackInEvent {
    Any, // match all
    SyncUpdate(client::state::SlidingSyncState),
}

impl PartialOrd for JackInEvent {
    fn partial_cmp(&self, _other: &Self) -> Option<std::cmp::Ordering> {
        None
    }
}

impl Eq for JackInEvent {}

impl PartialEq for JackInEvent {
    fn eq(&self, _other: &JackInEvent) -> bool {
        false
    }
}

// Let's define the component ids for our application
#[derive(Debug, Eq, PartialEq, PartialOrd, Clone, Hash)]
pub enum Id {
    Clock,
    DigitCounter,
    LetterCounter,
    Label,
    Logger,
    Status,
    Rooms,
    Details,
}

use structopt::StructOpt;

#[derive(Debug, StructOpt)]
#[structopt(name = "jack-in", about = "Your experimental sliding-sync jack into the matrix")]
struct Opt {
    /// The address of the sliding sync server to connect (probs the proxy)
    #[structopt(short, long, default_value = "http://localhost:8008", env = "JACKIN_SYNC_PROXY")]
    sliding_sync_proxy: String,

    /// Your access token to connect via the
    #[structopt(short, long, env = "JACKIN_TOKEN")]
    token: String,

    /// The userID associated with this access token
    #[structopt(short, long, env = "JACKIN_USER")]
    user: String,

    #[structopt(long)]
    /// Activate tracing and write the flamegraph to the specified file
    flames: Option<PathBuf>,
}

pub(crate) struct MatrixPoller(mpsc::Receiver<client::state::SlidingSyncState>);

impl tuirealm::listener::Poll<JackInEvent> for MatrixPoller {
    fn poll(&mut self) -> tuirealm::listener::ListenerResult<Option<Event<JackInEvent>>> {
        match self.0.try_recv() {
            Ok(v) => Ok(Some(Event::User(JackInEvent::SyncUpdate(v)))),
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            _ => Err(tuirealm::listener::ListenerError::ListenerDied),
        }
    }
}

fn setup_flames(path: &Path) -> impl Drop {
    let (flame_layer, _guard) = FlameLayer::with_file(path).expect("Couldn't write flamegraph");

    tracing_subscriber::registry().with(flame_layer).init();
    _guard
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::from_args();

    let user_id: OwnedUserId = opt.user.clone().parse()?;
    let device_id: OwnedDeviceId = "XdftAsd".into();

    if let Some(ref p) = opt.flames {
        setup_flames(p.as_path());
    } else {
        // Configure log

        //tracing_subscriber::fmt::init();
        #[cfg(feature = "file-logging")]
        {
            use log4rs::{
                append::file::FileAppender,
                config::{Appender, Config, Logger, Root},
                encode::pattern::PatternEncoder,
            };
            use tracing::level_filters::LevelFilter;

            let file = FileAppender::builder()
                .encoder(Box::new(PatternEncoder::default()))
                .build("jack-in.log")
                .unwrap();

            let config = Config::builder()
                .appender(Appender::builder().build("file", Box::new(file)))
                .logger(
                    Logger::builder()
                        .appender("file")
                        .build("matrix_sdk::sliding_sync", LevelFilter::Trace),
                )
                .logger(
                    Logger::builder()
                        .appender("file")
                        .build("matrix_sdk::http_client", LevelFilter::Debug),
                )
                .logger(
                    Logger::builder()
                        .appender("file")
                        .build("matrix_sdk_base::sliding_sync", LevelFilter::Debug),
                )
                .logger(Logger::builder().appender("file").build("reqwest", LevelFilter::Trace))
                .logger(Logger::builder().appender("file").build("matrix_sdk", LevelFilter::Warn))
                .build(Root::builder().build(LevelFilter::Error))
                .unwrap();

            let handle =
                log4rs::init_config(config).expect("Logging with log4rs failed to initialize");
        }
        #[cfg(not(feature = "file-logging"))]
        {
            tui_logger::init_logger(LevelFilter::Trace).expect("Could not set up logging");
            tui_logger::set_default_level(LevelFilter::Warn);
            tui_logger::set_level_for_target("matrix_sdk", LevelFilter::Warn);
        }
    }

    let client = Client::builder().server_name(user_id.server_name()).build().await?;
    let session = Session {
        access_token: opt.token.clone(),
        refresh_token: None,
        user_id: user_id.clone(),
        device_id,
    };
    client.restore_login(session).await?;
    let sliding_client = client.clone();
    let proxy = opt.sliding_sync_proxy.clone();

    let (tx, mut rx) = mpsc::channel(100);
    let model_tx = tx.clone();

    tokio::spawn(async move {
        if let Err(e) = client::run_client(sliding_client, proxy, tx).await {
            warn!("Running the client failed: {:#?}", e);
        }
    });

    let start_sync =
        rx.recv().await.ok_or_else(|| eyre!("failure getting the sliding sync state"))?;
    // ensure client still works as normal: fetch user info:
    let display_name = client
        .account()
        .get_display_name()
        .await?
        .map(|s| format!("{} ({})", s, user_id))
        .unwrap_or_else(|| format!("{}", user_id));
    let poller = MatrixPoller(rx);
    let mut model = Model::new(start_sync, model_tx, poller);
    model.set_title(format!("{} via {}", display_name, opt.sliding_sync_proxy));
    run_ui(model).await;

    Ok(())
}

async fn run_ui(mut model: Model) {
    // Enter alternate screen
    let _ = model.terminal.enter_alternate_screen();
    let _ = model.terminal.enable_raw_mode();
    // Main loop
    // NOTE: loop until quit; quit is set in update if AppClose is received from
    // counter
    while !model.quit {
        // Tick
        match model.app.tick(PollStrategy::Once) {
            Err(err) => {
                model.set_title(format!("Application error: {}", err));
            }
            Ok(messages) if !messages.is_empty() => {
                // NOTE: redraw if at least one msg has been processed
                model.redraw = true;
                for msg in messages.into_iter() {
                    let mut msg = Some(msg);
                    while msg.is_some() {
                        msg = model.update(msg);
                    }
                }
            }
            _ => {}
        }
        // Redraw
        if model.redraw {
            model.view();
            model.redraw = false;
        }
    }
    // Terminate terminal
    let _ = model.terminal.leave_alternate_screen();
    let _ = model.terminal.disable_raw_mode();
    let _ = model.terminal.clear_screen();
}
