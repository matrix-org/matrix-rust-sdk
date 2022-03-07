use std::time::Duration;

pub mod handler;
// For this dummy application we only need two IO event
#[derive(Debug, Clone)]
pub enum IoEvent {
    Initialize,      // Launch to initialize the application
    Sleep(Duration), // Just take a little break
}
