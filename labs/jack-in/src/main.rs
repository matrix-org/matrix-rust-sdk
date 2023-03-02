//! ## Jack-in
//!
//! a demonstration and debugging implementation TUI client for sliding sync

use std::{path::Path, time::Duration};

use app_dirs2::{app_root, AppDataType, AppInfo};
use clap::Parser;
use dialoguer::{theme::ColorfulTheme, Password};
use eyeball_im::VectorDiff;
use eyre::{eyre, Result};
use matrix_sdk::{
    config::RequestConfig,
    room::timeline::TimelineItem,
    ruma::{OwnedRoomId, OwnedUserId},
    Client,
};
use matrix_sdk_sled::make_store_config;
use sanitize_filename_reader_friendly::sanitize;
use tracing::{error, info, log};
use tracing_flame::FlameLayer;
use tracing_subscriber::prelude::*;
use tuirealm::{application::PollStrategy, Event, Update};

const APP_INFO: AppInfo = AppInfo { name: "jack-in", author: "Matrix-Rust-SDK Core Team" };

// -- internal
mod app;
mod client;
mod components;
mod config;
use app::model::Model;
use config::{Opt, SlidingSyncConfig};
use tokio::sync::mpsc;

// Let's define the messages handled by our app. NOTE: it must derive
// `PartialEq`
#[derive(PartialEq, Eq)]
pub enum Msg {
    AppClose,
    Clock,
    RoomsBlur,
    DetailsBlur,
    TextBlur,
    SelectRoom(Option<OwnedRoomId>),
    SendMessage(String),
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum JackInEvent {
    Any, // match all
    SyncUpdate(client::state::SlidingSyncState),
    RoomDataUpdate(VectorDiff<TimelineItem>),
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
    TextMessage,
    Logger,
    Status,
    Rooms,
    Details,
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
    let opt = Opt::parse();

    let user_id: OwnedUserId = opt.user.clone().parse()?;

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

            let file = FileAppender::builder()
                .encoder(Box::new(PatternEncoder::default()))
                .build("jack-in.log")
                .unwrap();

            let config = Config::builder()
                .appender(Appender::builder().build("file", Box::new(file)))
                .logger(
                    Logger::builder()
                        .appender("file")
                        .build("matrix_sdk::sliding_sync", log::LevelFilter::Trace),
                )
                .logger(
                    Logger::builder()
                        .appender("file")
                        .build("matrix_sdk::http_client", log::LevelFilter::Debug),
                )
                .logger(
                    Logger::builder()
                        .appender("file")
                        .build("matrix_sdk_base::sliding_sync", log::LevelFilter::Debug),
                )
                .logger(
                    Logger::builder().appender("file").build("reqwest", log::LevelFilter::Trace),
                )
                .logger(
                    Logger::builder().appender("file").build("matrix_sdk", log::LevelFilter::Warn),
                )
                .build(Root::builder().build(log::LevelFilter::Error))
                .unwrap();

            log4rs::init_config(config).expect("Logging with log4rs failed to initialize");
        }
        #[cfg(not(feature = "file-logging"))]
        {
            tui_logger::init_logger(log::LevelFilter::Trace).unwrap();
            // Set default level for unknown targets to Trace
            tui_logger::set_default_level(log::LevelFilter::Warn);

            for pair in opt.log.split(',') {
                if let Some((name, lvl)) = pair.split_once('=') {
                    let level = match lvl.to_lowercase().as_str() {
                        "trace" => log::LevelFilter::Trace,
                        "debug" => log::LevelFilter::Debug,
                        "info" => log::LevelFilter::Info,
                        "warn" => log::LevelFilter::Warn,
                        "error" => log::LevelFilter::Error,
                        // nothing means error
                        _ => continue,
                    };
                    tui_logger::set_level_for_target(name, level);
                } else {
                    let level = match pair.to_lowercase().as_str() {
                        "trace" => log::LevelFilter::Trace,
                        "debug" => log::LevelFilter::Debug,
                        "info" => log::LevelFilter::Info,
                        "warn" => log::LevelFilter::Warn,
                        "error" => log::LevelFilter::Error,
                        // nothing means error
                        _ => continue,
                    };
                    tui_logger::set_default_level(level);
                }
            }
        }
    }

    let data_path = app_root(AppDataType::UserData, &APP_INFO)?.join(sanitize(user_id.as_str()));
    if opt.fresh {
        // drop the database first;
        std::fs::remove_dir_all(&data_path)?;
    }
    std::fs::create_dir_all(&data_path)?;
    let store_config = make_store_config(&data_path, opt.store_pass.as_deref()).await?;
    let request_config = RequestConfig::default().timeout(Duration::from_secs(90));

    let client = Client::builder()
        .user_agent("jack-in")
        .server_name(user_id.server_name())
        .request_config(request_config)
        .store_config(store_config)
        .build()
        .await?;

    let session_key = b"jackin::session_token";

    if let Some(session) = client
        .store()
        .get_custom_value(session_key)
        .await?
        .map(|v| serde_json::from_slice(&v))
        .transpose()?
    {
        info!("Restoring session from store");
        client.restore_session(session).await?;
    } else {
        let theme = ColorfulTheme::default();
        let password = match opt.password {
            Some(ref pw) => pw.clone(),
            _ => Password::with_theme(&theme)
                .with_prompt(format!("Password for {user_id:} :"))
                .interact()?,
        };
        client.login_username(&user_id, &password).await?;
    }

    if let Some(session) = client.session() {
        client.store().set_custom_value(session_key, serde_json::to_vec(&session)?).await?;
    }

    let sliding_client = client.clone();

    let (tx, mut rx) = mpsc::channel(100);
    let model_tx = tx.clone();
    let sliding_sync_proxy = opt.sliding_sync.proxy.clone();

    tokio::spawn(async move {
        if let Err(e) = client::run_client(sliding_client, tx, opt.sliding_sync).await {
            error!("Running the client failed: {:#?}", e);
        }
    });

    let start_sync =
        rx.recv().await.ok_or_else(|| eyre!("failure getting the sliding sync state"))?;
    // ensure client still works as normal: fetch user info:
    let display_name = client
        .account()
        .get_display_name()
        .await?
        .map(|s| format!("{s} ({user_id})"))
        .unwrap_or_else(|| format!("{user_id}"));
    let poller = MatrixPoller(rx);
    let mut model = Model::new(start_sync, model_tx, poller, client);
    model.set_title(format!("{display_name} via {sliding_sync_proxy}"));
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
                model.set_title(format!("Application error: {err}"));
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
