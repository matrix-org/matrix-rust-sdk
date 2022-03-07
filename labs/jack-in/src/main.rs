use std::io;
use tui::{backend::CrosstermBackend, Terminal};

use structopt::StructOpt;

#[derive(Debug, StructOpt)]
#[structopt(name = "jack-in", about = "Your experimental sliding-sync jack into the matrix")]
struct Opt {
    /// Your access token to connect via the 
    #[structopt(short, long)]
    token: String,
}

fn main() -> Result<(), io::Error> {
    let opt = Opt::from_args();
    println!("{:?}", opt);
    let stdout = io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    Ok(())
}