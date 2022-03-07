use std::io;
use tui::{backend::CrosstermBackend, Terminal};

use structopt::StructOpt;

use std::sync::Arc;

use eyre::Result;
use log::LevelFilter;
use jack_in::app::App;
use jack_in::io::handler::IoAsyncHandler;
use jack_in::io::IoEvent;
use jack_in::start_ui;


#[derive(Debug, StructOpt)]
#[structopt(name = "jack-in", about = "Your experimental sliding-sync jack into the matrix")]
struct Opt {
    /// The address of the sliding sync server to connect (probs the proxy)
    #[structopt(short, long, default_value="http://localhost:8008")]
    matrix_endpoint: String,
    /// Your access token to connect via the 
    #[structopt(short, long, env)]
    token: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::from_args();
    println!("{:?}", opt);

    let (sync_io_tx, mut sync_io_rx) = tokio::sync::mpsc::channel::<IoEvent>(100);

    // We need to share the App between thread
    let app = Arc::new(tokio::sync::Mutex::new(App::new(sync_io_tx.clone())));
    let app_ui = Arc::clone(&app);

    // Configure log
    tui_logger::init_logger(LevelFilter::Debug).unwrap();
    tui_logger::set_default_level(log::LevelFilter::Debug);

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