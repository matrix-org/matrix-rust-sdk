use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use futures_util::FutureExt as _;
use layout::Flex;
use matrix_sdk::{
    encryption::recovery::{RecoveryError, RecoveryState},
    Client,
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use throbber_widgets_tui::{Throbber, ThrobberState};
use tokio::task::JoinHandle;

use super::{create_centered_throbber_area, ShouldExit};

#[derive(Debug)]
pub struct DefaultRecoveryView {
    client: Client,
    recovery_state: RecoveryState,
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

        Self { client, state, recovery_state, mode: Mode::default() }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ShouldExit {
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
                            MenuEntries::Recovery => {
                                // TODO: Enable the client here.
                                // let client = self.client.clone();

                                if matches!(self.recovery_state, RecoveryState::Disabled) {
                                    let enable_task = tokio::spawn(async move {
                                        // TODO: Enable the client here.
                                        // client.encryption().recovery().enable().await
                                        tokio::time::sleep(Duration::from_secs(3)).await;
                                        Ok("HELLO WORLD".to_owned())
                                    });

                                    self.mode = Mode::Enabling {
                                        enable_task,
                                        throbber_state: ThrobberState::default(),
                                    };
                                } else {
                                    let disable_task = tokio::spawn(async move {
                                        // TODO: Enable the client here.
                                        // client.encryption().recovery().disable().await;
                                        tokio::time::sleep(Duration::from_secs(3)).await;
                                        Ok(())
                                    });

                                    self.mode = Mode::Disabling {
                                        disable_task,
                                        throbber_state: ThrobberState::default(),
                                    };
                                }
                            }
                            MenuEntries::KeyStorage => {
                                // TODO: Support the enabling and disabling of
                                // backups.
                            }
                        }
                    }

                    No
                }
                _ => No,
            },
            Mode::Enabling { .. } | Mode::Disabling { .. } => No,
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
            Enabling { throbber_state, .. } | Disabling { throbber_state, .. } => {
                throbber_state.calc_next()
            }
            Default | Done { .. } => {}
        }
    }

    fn update_state(&mut self) {
        use Mode::*;

        let recovery_state = self.client.encryption().recovery().state();

        self.recovery_state = recovery_state;

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
            RecoveryState::Unknown => ListItem::new("Recovery [?]").style(Style::default().dim()),
            RecoveryState::Enabled => ListItem::new("Recovery [x]").style(style),
            RecoveryState::Disabled | RecoveryState::Incomplete => {
                ListItem::new("Recovery [ ]").style(style)
            }
        };

        let backups =
            ListItem::new("Key storage [ ] (not yet supported)").style(Style::default().dim());
        let list = List::new(vec![recovery_item, backups])
            .highlight_symbol("> ")
            .highlight_spacing(ratatui::widgets::HighlightSpacing::Always);

        StatefulWidget::render(list, area, buf, &mut self.state);

        match &mut self.mode {
            Mode::Default => {}
            Mode::Enabling { throbber_state, .. } | Mode::Disabling { throbber_state, .. } => {
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

                let text = match result {
                    DoneResult::Enabling(Ok(recovery_key)) => {
                        format!("Recovery has been enabled:\n{recovery_key}")
                    }
                    DoneResult::Enabling(Err(error)) => {
                        format!("Failed to enable recovery: {error:?}")
                    }
                    DoneResult::Disabling(Ok(())) => {
                        format!("Recovery has been disabled")
                    }
                    DoneResult::Disabling(Err(error)) => {
                        format!("Failed to disable recovery: {error:?}")
                    }
                };

                Paragraph::new(text).centered().block(block).render(popup, buf);
            }
        }
    }
}
