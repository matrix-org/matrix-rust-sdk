use std::io;
use tui::{backend::CrosstermBackend, Terminal};

use structopt::StructOpt;

use std::sync::Arc;

use eyre::Result;
use log::LevelFilter;
use jack_in::app::App;
use jack_in::io::handler::IoAsyncHandler;
use jack_in::io::IoEvent;
use jack_in::{start_ui, run_client};
use matrix_sdk::{Client, Session};
use matrix_sdk_common::ruma::{ UserId, DeviceId };
use log::warn;


#[derive(Debug, StructOpt)]
#[structopt(name = "jack-in", about = "Your experimental sliding-sync jack into the matrix")]
struct Opt {
    /// The address of the sliding sync server to connect (probs the proxy)
    #[structopt(short, long, default_value="http://localhost:8008", env="JACKIN_SYNC_PROXY")]
    sliding_sync_proxy: String,

    /// The address of the original homeserver behind the proxy
    #[structopt(short, long, env="JACKIN_USER")]
    user: String,

    /// Your access token to connect via the 
    #[structopt(short, long, env="JACKIN_TOKEN")]
    token: String,
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

    let (sync_io_tx, mut sync_io_rx) = tokio::sync::mpsc::channel::<IoEvent>(10);

    // We need to share the App between thread
    let app = Arc::new(tokio::sync::Mutex::new(App::new(sync_io_tx.clone())));
    let app_ui = Arc::clone(&app);
    let client_app = Arc::clone(&app);


    let client = Client::builder().user_id(&user_id).build().await?;
    let session = Session {
        access_token: opt.token.clone(),
        user_id,
        device_id,
    };
    client.restore_login(session).await?;

    tokio::spawn(async move {
        if let Err(e) = run_client(client, opt.sliding_sync_proxy.clone(), client_app).await {
            warn!("Running the client failed: {:#?}", e);
        }
    });

    // Handle IO in a specifc thread
    tokio::spawn(async move {
        let mut handler = IoAsyncHandler::new(app);
        while let Some(io_event) = sync_io_rx.recv().await {
            handler.handle_io_event(io_event).await;
        }
    });

    start_ui(&app_ui).await?;

    Ok(())
}