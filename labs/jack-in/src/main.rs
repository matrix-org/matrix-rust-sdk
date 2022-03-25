//! ## Demo
//!
//! `Demo` shows how to use tui-realm in a real case

use eyre::{eyre, Result};
use log::{warn, LevelFilter};
use matrix_sdk::{Client, Session};
use matrix_sdk_common::ruma::{DeviceId, RoomId, UserId};
use tuirealm::application::PollStrategy;
use tuirealm::{AttrValue, Attribute, Event, Update};
// -- internal
mod app;
mod client;
mod components;
use app::model::Model;
use tokio::sync::mpsc;

// Let's define the messages handled by our app. NOTE: it must derive
// `PartialEq`
#[derive(PartialEq)]
pub enum Msg {
    AppClose,
    Clock,
    RoomsBlur,
    DetailsBlur,
    SelectRoom(Option<Box<RoomId>>),
}

#[derive(Eq, PartialEq, PartialOrd, Clone)]
pub enum JackInEvent {
    Any, // match all
    SyncUpdate(client::state::SlidingSyncState),
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

    /// The address of the original homeserver behind the proxy
    #[structopt(short, long, env = "JACKIN_USER")]
    user: String,

    /// Your access token to connect via the
    #[structopt(short, long, env = "JACKIN_TOKEN")]
    token: String,
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

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::from_args();

    let user_id: Box<UserId> = opt.user.clone().parse()?;
    let device_id: Box<DeviceId> = "XdftAsd".into();

    // Configure log

    //tracing_subscriber::fmt::init();
    tui_logger::init_logger(LevelFilter::Trace).expect("Could not set up logging");
    tui_logger::set_default_level(log::LevelFilter::Warn);
    tui_logger::set_level_for_target("matrix_sdk::client", log::LevelFilter::Warn);

    let title = format!("{} via {}", user_id, opt.sliding_sync_proxy);

    let client = Client::builder().user_id(&user_id).build().await?;
    let session = Session { access_token: opt.token.clone(), user_id, device_id };
    client.restore_login(session).await?;

    let (tx, mut rx) = mpsc::channel(100);
    let model_tx = tx.clone();

    tokio::spawn(async move {
        if let Err(e) = client::run_client(client, opt.sliding_sync_proxy.clone(), tx).await {
            warn!("Running the client failed: {:#?}", e);
        }
    });

    let start_sync =
        rx.recv().await.ok_or_else(|| eyre!("failure getting the sliding sync state"))?;
    let poller = MatrixPoller(rx);
    let mut model = Model::new(start_sync, model_tx, poller);
    model.set_title(title);
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
            Ok(messages) if messages.len() > 0 => {
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
