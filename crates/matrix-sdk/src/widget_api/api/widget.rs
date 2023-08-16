use tokio::sync::mpsc::{UnboundedReceiver as Receiver, UnboundedSender as Sender};

#[derive(Debug)]
pub struct Widget {
    pub info: Info,
    pub comm: Comm,
}

#[derive(Debug)]
pub struct Info {
    pub id: String,
    pub init_on_load: bool,
}

#[derive(Debug)]
pub struct Comm {
    pub from: Receiver<String>,
    pub to: Sender<String>,
}

impl Comm {
    pub fn sink(&self) -> Sender<String> {
        self.to.clone()
    }

    pub async fn recv(&mut self) -> Option<String> {
        self.from.recv().await
    }
}
