use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use matrix_sdk::Client;
use matrix_sdk_ui::sync_service::{self, SyncService};
use ratatui::{
    prelude::*,
    widgets::{HighlightSpacing, *},
};

// TODO: This replicates a lot of the logic the details view has, we should make
// a generic tab popout widget to share a bit of logic here.

enum MenuEntries {
    Sync = 0,
    SendQueue = 1,
}

impl From<usize> for MenuEntries {
    fn from(value: usize) -> Self {
        match value {
            0 => MenuEntries::Sync,
            1 => MenuEntries::SendQueue,
            _ => unreachable!("The developer settings view has only 2 options"),
        }
    }
}

pub struct DeveloperSettingsView {
    client: Client,
    sync_service: Arc<SyncService>,
    state: ListState,
}

impl DeveloperSettingsView {
    pub fn new(client: Client, sync_service: Arc<SyncService>) -> Self {
        let state = ListState::default().with_selected(Some(0));
        Self { client, sync_service, state }
    }

    pub async fn handle_key_press(&mut self, key: KeyEvent) {
        use MenuEntries::*;

        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.state.select_next();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.state.select_previous();
            }

            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(selected) = self.state.selected() {
                    match selected.into() {
                        Sync => {
                            let sync_service = &self.sync_service;

                            match sync_service.state().get() {
                                sync_service::State::Running => sync_service.stop().await,
                                sync_service::State::Idle
                                | sync_service::State::Terminated
                                | sync_service::State::Error(_)
                                | sync_service::State::Offline => sync_service.start().await,
                            }
                        }
                        SendQueue => {
                            let send_queue = self.client.send_queue();
                            let enabled = send_queue.is_enabled();
                            send_queue.set_enabled(!enabled).await
                        }
                    }
                }
            }

            _ => (),
        }
    }
}

impl Widget for &mut DeveloperSettingsView {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        let sync_item = match self.sync_service.state().get() {
            sync_service::State::Running => ListItem::new("Sync [x]"),
            sync_service::State::Idle
            | sync_service::State::Terminated
            | sync_service::State::Error(_)
            | sync_service::State::Offline => ListItem::new("Sync [ ]"),
        };

        let send_queue_item = if self.client.send_queue().is_enabled() {
            ListItem::new("Send Queue [x]")
        } else {
            ListItem::new("Send Queue [ ]")
        };

        let list = List::new(vec![sync_item, send_queue_item])
            .highlight_symbol("> ")
            .highlight_spacing(HighlightSpacing::Always);

        StatefulWidget::render(list, area, buf, &mut self.state);
    }
}
