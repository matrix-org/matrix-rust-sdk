use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use futures_util::FutureExt as _;
use layout::Flex;
use matrix_sdk::{encryption::recovery::RecoveryError, Client};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use throbber_widgets_tui::{Throbber, ThrobberState};
use tokio::task::JoinHandle;

use super::{create_centered_throbber_area, ShouldExit};

#[derive(Debug)]
pub struct DisabledView {
    client: Client,
    state: ListState,
    mode: Mode,
}

#[derive(Debug, Default)]
enum Mode {
    #[default]
    Default,
    Enabling {
        enable_task: JoinHandle<Result<String, RecoveryError>>,
        throbber_state: ThrobberState,
    },
    Done {
        result: Result<String, RecoveryError>,
    },
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

impl DisabledView {
    pub fn new(client: Client) -> Self {
        let mut state = ListState::default();
        state.select_first();

        Self { client, state, mode: Default::default() }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ShouldExit {
        use ShouldExit::*;

        if key.kind != KeyEventKind::Press {
            return No;
        }

        match self.mode {
            Mode::Default => match key.code {
                KeyCode::Esc => Yes,
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
                            MenuEntries::Recovery => {
                                // let client = self.client.clone();

                                let enable_task = tokio::spawn(async move {
                                    // client.encryption().recovery().enable().await
                                    tokio::time::sleep(Duration::from_secs(3)).await;
                                    Ok("HELLO WORLD".to_owned())
                                });

                                self.mode = Mode::Enabling {
                                    enable_task,
                                    throbber_state: ThrobberState::default(),
                                };
                            }
                            MenuEntries::KeyStorage => todo!("Enable or disable backus"),
                        }
                    }

                    No
                }
                _ => No,
            },
            Mode::Enabling { .. } => No,
            Mode::Done { .. } => match key.code {
                _ => {
                    self.mode = Mode::Default;
                    OnlySubScreen
                }
            },
        }
    }

    pub fn on_tick(&mut self) {
        use Mode::*;

        match &mut self.mode {
            Enabling { throbber_state, .. } => throbber_state.calc_next(),
            Default | Done { .. } => {}
        }
    }

    fn update_state(&mut self) {
        use Mode::*;

        match &mut self.mode {
            Default => {}
            Enabling { enable_task, .. } => {
                if enable_task.is_finished() {
                    let result = enable_task
                        .now_or_never()
                        .expect("The task should have finished, we checked it")
                        .expect("The recovery enabling task should neve panic");
                    self.mode = Done { result };
                }
            }
            _ => {}
        }
    }
}

impl Widget for &mut DisabledView {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        self.update_state();

        let style = match &self.mode {
            Mode::Default => Style::default(),
            Mode::Enabling { .. } | Mode::Done { .. } => Style::default().dim(),
        };

        let recovery_item = ListItem::new("Recovery [ ]").style(style);
        let backups = ListItem::new("Key storage [x]").style(style);
        let list = List::new(vec![recovery_item, backups])
            .highlight_symbol("> ")
            .highlight_spacing(ratatui::widgets::HighlightSpacing::Always);

        StatefulWidget::render(list, area, buf, &mut self.state);

        match &mut self.mode {
            Mode::Default => {}
            Mode::Enabling { throbber_state, .. } => {
                let throbber = Throbber::default()
                    .label("Recovering")
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

                match result {
                    Ok(recovery_key) => {
                        Paragraph::new(format!("Recovery has been enabled:\n\t{recovery_key}"))
                            .centered()
                            .block(block)
                            .render(popup, buf);
                    }
                    Err(error) => {
                        Paragraph::new(format!("Failed to enable recovery: {error:?}"))
                            .block(block)
                            .centered()
                            .render(popup, buf);
                    }
                }
            }
        }
    }
}
