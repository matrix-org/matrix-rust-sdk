use crossterm::event::{KeyCode, KeyEvent};
use futures_util::FutureExt as _;
use matrix_sdk::{encryption::recovery::RecoveryError, Client};
use ratatui::{
    prelude::*,
    widgets::{Block, Paragraph},
};
use throbber_widgets_tui::{Throbber, ThrobberState};
use tokio::task::JoinHandle;
use tui_textarea::TextArea;

use super::ShouldExit;
use crate::widgets::recovery::create_centered_throbber_area;

#[derive(Debug)]
pub struct RecoveringView {
    client: Client,
    mode: Mode,
}

#[derive(Debug)]
enum Mode {
    Recovering {
        recovery_task: JoinHandle<Result<(), RecoveryError>>,
        throbber_state: ThrobberState,
    },
    Inputting {
        recovery_text_area: TextArea<'static>,
    },
    Done {
        result: Result<(), RecoveryError>,
    },
}

impl RecoveringView {
    pub fn new(client: Client) -> Self {
        let mut recovery_text_area = TextArea::default();

        recovery_text_area.set_cursor_line_style(Style::default());
        recovery_text_area.set_mask_char('\u{2022}'); //U+2022 BULLET (•)
        recovery_text_area.set_placeholder_text("To enable recovery enter the recover key");

        recovery_text_area.set_style(Style::default().fg(Color::LightGreen));
        recovery_text_area.set_block(Block::default());

        Self { client, mode: Mode::Inputting { recovery_text_area } }
    }

    fn update(&mut self) {
        use Mode::*;

        match &mut self.mode {
            Recovering { recovery_task, .. } => {
                if recovery_task.is_finished() {
                    let result = recovery_task
                        .now_or_never()
                        .expect("The task should have finished, we checked it")
                        .expect("The recovery enabling task should neve panic");
                    self.mode = Done { result };
                }
            }
            Inputting { .. } | Done { .. } => {}
        }
    }

    pub fn on_tick(&mut self) {
        use Mode::*;

        match &mut self.mode {
            Recovering { throbber_state, .. } => throbber_state.calc_next(),
            Inputting { .. } | Done { .. } => {}
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ShouldExit {
        use KeyCode::*;
        use Mode::*;
        use ShouldExit::*;

        match &mut self.mode {
            Recovering { .. } => No,

            Inputting { recovery_text_area } => {
                match (key.modifiers, key.code) {
                    (_, Esc) => Yes,
                    (_, Enter) => {
                        // We expect a single line since pressing enter gets us here, still, let's
                        // just join all the lines into a single one.
                        let recovery_key = recovery_text_area.lines().join("");
                        let client = self.client.clone();

                        let recovery_task = tokio::spawn(async move {
                            client.encryption().recovery().recover(recovery_key.trim()).await
                        });

                        self.mode =
                            Recovering { recovery_task, throbber_state: ThrobberState::default() };

                        No
                    }
                    _ => {
                        recovery_text_area.input(key);

                        No
                    }
                }
            }
            Done { .. } => match (key.modifiers, key.code) {
                _ => OnlySubScreen,
            },
        }
    }
}

impl Widget for &mut RecoveringView {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        use Mode::*;

        self.update();

        match &mut self.mode {
            Recovering { throbber_state, .. } => {
                let throbber = Throbber::default()
                    .label("Recovering")
                    .throbber_set(throbber_widgets_tui::BRAILLE_EIGHT_DOUBLE);
                let centered_area = create_centered_throbber_area(area);
                StatefulWidget::render(throbber, centered_area, buf, throbber_state);
            }

            Inputting { recovery_text_area } => {
                let [left, right] =
                    Layout::horizontal([Constraint::Length(14), Constraint::Length(50)])
                        .areas(area);

                Paragraph::new("Recovery key: ").render(left, buf);
                recovery_text_area.render(right, buf);
            }

            Done { result } => {
                let constraints = [Constraint::Fill(1), Constraint::Min(3), Constraint::Fill(1)];
                let [_top, middle, _bottom] = Layout::vertical(constraints).areas(area);

                match result {
                    Ok(_) => {
                        Paragraph::new(format!("Done recovering\n\nPress any key to continue"))
                            .centered()
                            .render(middle, buf);
                    }
                    Err(error) => {
                        Paragraph::new(format!(
                            "Error recovering: {error:?}\n\nPress any key to continue"
                        ))
                        .centered()
                        .render(middle, buf);
                    }
                }
            }
        }
    }
}
