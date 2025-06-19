use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use futures_util::FutureExt as _;
use layout::Flex;
use matrix_sdk::{
    Client,
    encryption::{
        backups::BackupState,
        recovery::{RecoveryError, RecoveryState},
    },
};
use matrix_sdk_common::executor::spawn;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use throbber_widgets_tui::{Throbber, ThrobberState};
use tokio::task::JoinHandle;

use super::{ShouldExit, create_centered_throbber_area};

#[derive(Debug)]
pub struct DefaultRecoveryView {
    client: Client,
    recovery_state: RecoveryState,
    backup_info: BackupInfo,
    state: ListState,
    mode: Mode,
}

#[derive(Debug)]
struct BackupInfo {
    backup_state: BackupState,
    backup_exists: Arc<AtomicBool>,
    backup_update_task: JoinHandle<()>,
}

impl Drop for BackupInfo {
    fn drop(&mut self) {
        self.backup_update_task.abort();
    }
}

#[derive(Debug, Default)]
enum Mode {
    #[default]
    Default,
    Enabling {
        enable_task: JoinHandle<Result<String, RecoveryError>>,
        throbber_state: ThrobberState,
    },
    Disabling {
        disable_task: JoinHandle<Result<(), RecoveryError>>,
        throbber_state: ThrobberState,
    },
    Done {
        result: DoneResult,
    },
}

#[derive(Debug)]
enum DoneResult {
    Enabling(Result<String, RecoveryError>),
    Disabling(Result<(), RecoveryError>),
}

enum MenuEntries {
    Recovery = 0,
    KeyStorage = 1,
}

impl From<usize> for MenuEntries {
    fn from(value: usize) -> Self {
        match value {
            0 => MenuEntries::Recovery,
            1 => MenuEntries::KeyStorage,
            _ => unreachable!("The recovery disabled view has only 2 options"),
        }
    }
}

impl DefaultRecoveryView {
    pub fn new(client: Client) -> Self {
        let mut state = ListState::default();
        state.select_first();

        let recovery_state = client.encryption().recovery().state();
        let backup_state = client.encryption().backups().state();
        let backup_exists = Arc::new(AtomicBool::default());

        let backup_update_task = spawn({
            let client = client.clone();
            let backup_exists = backup_exists.clone();

            async move {
                loop {
                    if let Ok(exists) = client.encryption().backups().fetch_exists_on_server().await
                    {
                        backup_exists.store(exists, Ordering::SeqCst);
                    }

                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        });

        let backup_info = BackupInfo { backup_state, backup_exists, backup_update_task };

        Self { client, state, recovery_state, backup_info, mode: Mode::default() }
    }

    fn handle_recovery_action(&mut self) {
        let client = self.client.clone();

        if matches!(self.recovery_state, RecoveryState::Disabled) {
            let enable_task = spawn(async move { client.encryption().recovery().enable().await });

            self.mode = Mode::Enabling { enable_task, throbber_state: ThrobberState::default() };
        } else {
            let disable_task = spawn(async move {
                // TODO: Handle errors here?
                let _ = client.encryption().recovery().disable().await;
                Ok(())
            });

            self.mode = Mode::Disabling { disable_task, throbber_state: ThrobberState::default() };
        }
    }

    async fn handle_backup_action(&mut self) {
        let backup_state = self.backup_info.backup_state;
        let backup_exists = self.backup_info.backup_exists.load(Ordering::SeqCst);

        match (backup_state, backup_exists) {
            (BackupState::Unknown, false) => {
                let _ = self.client.encryption().backups().create().await;
            }
            (BackupState::Unknown, true) | (BackupState::Enabled, _) => {
                let _ = self.client.encryption().backups().disable_and_delete().await;
                self.backup_info.backup_exists.store(false, Ordering::SeqCst);
            }
            (BackupState::Creating, _)
            | (BackupState::Enabling, _)
            | (BackupState::Resuming, _)
            | (BackupState::Downloading, _)
            | (BackupState::Disabling, _) => {}
        }
    }

    pub async fn handle_key(&mut self, key: KeyEvent) -> ShouldExit {
        use ShouldExit::*;

        if key.kind != KeyEventKind::Press {
            return No;
        }

        match self.mode {
            Mode::Default => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => Yes,
                KeyCode::Char('j') | KeyCode::Down => {
                    self.state.select_next();
                    No
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.state.select_previous();
                    No
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    if let Some(selected) = self.state.selected() {
                        match selected.into() {
                            MenuEntries::Recovery => self.handle_recovery_action(),
                            MenuEntries::KeyStorage => self.handle_backup_action().await,
                        }
                    }

                    No
                }
                _ => No,
            },
            Mode::Enabling { .. } | Mode::Disabling { .. } => No,
            Mode::Done { .. } => {
                self.mode = Mode::Default;
                OnlySubScreen
            }
        }
    }

    pub fn on_tick(&mut self) {
        use Mode::*;

        match &mut self.mode {
            Enabling { throbber_state, .. } | Disabling { throbber_state, .. } => {
                throbber_state.calc_next()
            }
            Default | Done { .. } => {}
        }
    }

    pub fn is_idle(&self) -> bool {
        match self.mode {
            Mode::Default => true,
            Mode::Enabling { .. } | Mode::Disabling { .. } | Mode::Done { .. } => false,
        }
    }

    fn update_state(&mut self) {
        use Mode::*;

        let recovery_state = self.client.encryption().recovery().state();
        let backup_state = self.client.encryption().backups().state();

        self.recovery_state = recovery_state;
        self.backup_info.backup_state = backup_state;

        match &mut self.mode {
            Default => {}
            // Check if the task enabling recovery is done, if so, let's go into the `Done` mode.
            Enabling { enable_task, .. } => {
                if enable_task.is_finished() {
                    let result = enable_task
                        .now_or_never()
                        .expect("The task should have finished, we checked it")
                        .expect("The recovery enabling task should neve panic");
                    self.mode = Done { result: DoneResult::Enabling(result) };
                }
            }
            Disabling { disable_task, .. } => {
                if disable_task.is_finished() {
                    let result = disable_task
                        .now_or_never()
                        .expect("The task should have finished, we checked it")
                        .expect("The recovery enabling task should neve panic");
                    self.mode = Done { result: DoneResult::Disabling(result) };
                }
            }

            // Done only transitions into another state if the user presses a button.
            Done { .. } => {}
        }
    }
}

impl Widget for &mut DefaultRecoveryView {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        self.update_state();

        let style = match &self.mode {
            Mode::Default => Style::default(),
            Mode::Enabling { .. } | Mode::Done { .. } | Mode::Disabling { .. } => {
                Style::default().dim()
            }
        };

        let recovery_item = match self.recovery_state {
            RecoveryState::Unknown => {
                ListItem::new("Recovery    [?]").style(Style::default().dim())
            }
            RecoveryState::Enabled => ListItem::new("Recovery    [x]").style(style),
            RecoveryState::Disabled | RecoveryState::Incomplete => {
                ListItem::new("Recovery    [ ]").style(style)
            }
        };

        let backup_state = self.backup_info.backup_state;
        let backup_exists = self.backup_info.backup_exists.load(Ordering::SeqCst);

        let backups = match (backup_state, backup_exists) {
            (BackupState::Unknown, true) => {
                ListItem::new("Key storage [~] (a backup exists but we don't have access to it)")
                    .dim()
            }
            (BackupState::Unknown, false) => ListItem::new("Key storage [ ]"),
            (BackupState::Creating, _)
            | (BackupState::Enabling, _)
            | (BackupState::Resuming, _) => ListItem::new("Key storage [x]").dim(),
            (BackupState::Enabled, true) => ListItem::new("Key storage [x]"),
            (BackupState::Enabled, false) => ListItem::new("Key storage [x]"),
            (BackupState::Downloading, _) | (BackupState::Disabling, _) => {
                ListItem::new("Key storage [ ]").dim()
            }
        };

        let list = List::new(vec![recovery_item, backups])
            .highlight_symbol("> ")
            .highlight_spacing(ratatui::widgets::HighlightSpacing::Always);

        StatefulWidget::render(list, area, buf, &mut self.state);

        match &mut self.mode {
            Mode::Default => {}
            Mode::Enabling { throbber_state, .. } => {
                let throbber = Throbber::default()
                    .label("Enabling recovery")
                    .throbber_set(throbber_widgets_tui::BRAILLE_EIGHT_DOUBLE);
                let centered_area = create_centered_throbber_area(area);
                StatefulWidget::render(throbber, centered_area, buf, throbber_state);
            }
            Mode::Disabling { throbber_state, .. } => {
                let throbber = Throbber::default()
                    .label("Disabling recovery")
                    .throbber_set(throbber_widgets_tui::BRAILLE_EIGHT_DOUBLE);
                let centered_area = create_centered_throbber_area(area);
                StatefulWidget::render(throbber, centered_area, buf, throbber_state);
            }

            Mode::Done { result } => {
                let vertical = Layout::vertical([
                    Constraint::Fill(1),
                    Constraint::Length(4),
                    Constraint::Fill(1),
                ])
                .flex(Flex::Center);
                let horizontal = Layout::horizontal([Constraint::Length(70)]).flex(Flex::Center);
                let [_, area, _] = vertical.areas(area);
                let [popup] = horizontal.areas(area);

                Clear.render(popup, buf);

                let block = Block::new().borders(Borders::all());

                let text = match result {
                    DoneResult::Enabling(Ok(recovery_key)) => {
                        format!("Recovery has been enabled:\n{recovery_key}")
                    }
                    DoneResult::Enabling(Err(error)) => {
                        format!("Failed to enable recovery: {error:?}")
                    }
                    DoneResult::Disabling(Ok(())) => "Recovery has been disabled".to_owned(),
                    DoneResult::Disabling(Err(error)) => {
                        format!("Failed to disable recovery: {error:?}")
                    }
                };

                Paragraph::new(text).centered().block(block).render(popup, buf);
            }
        }
    }
}
