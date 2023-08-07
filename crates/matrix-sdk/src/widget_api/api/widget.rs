use tokio::sync::mpsc::{UnboundedReceiver as Receiver, UnboundedSender as Sender};

#[derive(Debug)]
pub struct Widget<T> {
    pub info: Info,
    pub api: T,
    pub comm: Comm,
}

#[derive(Debug)]
pub struct Info {
    pub id: String,
    pub negotiate: bool,
}

#[derive(Debug)]
pub struct Comm {
    from: Receiver<String>,
    to: Sender<String>,
}

impl Comm {
    pub fn sink(&self) -> Sender<String> {
        self.to.clone()
    }

    pub async fn recv(&mut self) -> Option<String> {
        self.from.recv().await
    }
}
