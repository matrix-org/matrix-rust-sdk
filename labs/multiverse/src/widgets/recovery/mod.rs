use crossterm::event::{KeyCode, KeyEvent};
use matrix_sdk::{Client, encryption::recovery::RecoveryState};
use ratatui::prelude::*;
use recovering::RecoveringView;
use throbber_widgets_tui::{Throbber, ThrobberState};

mod default;
mod recovering;

use default::DefaultRecoveryView;

#[derive(Default)]
pub struct RecoveryView {}

impl RecoveryView {
    pub fn new() -> Self {
        Self {}
    }
}

impl RecoveryView {}

pub struct RecoveryViewState {
    client: Client,
    throbber_state: ThrobberState,
    mode: Mode,
}

#[derive(Debug, Default)]
enum Mode {
    #[default]
    Unknown,
    Incomplete {
        view: RecoveringView,
    },
    Default {
        view: DefaultRecoveryView,
    },
}

pub enum ShouldExit {
    No,
    OnlySubScreen,
    Yes,
}

impl RecoveryViewState {
    pub fn new(client: Client) -> Self {
        Self { client, throbber_state: ThrobberState::default(), mode: Mode::default() }
    }

    fn update_state(&mut self) {
        let recovery_state = self.client.encryption().recovery().state();

        match (&mut self.mode, recovery_state) {
            // We were in the unknown mode, showing a throbber, but now we figured out that
            // recovery either exists and there's nothing much to do, or we can enable it.
            //
            // Let's switch to our default view which allows recovery to be disabled or enabled.
            (Mode::Unknown, RecoveryState::Disabled | RecoveryState::Enabled) => {
                self.mode = Mode::Default { view: DefaultRecoveryView::new(self.client.clone()) };
            }

            // The recovery state changed to incomplete, we go into the incomplete view so users
            // can input the recovery key or reset recovery.
            (Mode::Unknown, RecoveryState::Incomplete) => {
                let view = RecoveringView::new(self.client.clone());
                self.mode = Mode::Incomplete { view }
            }

            // We were showing the incomplete view but someone disabled recovery on another device,
            // let's change the screen to reflect that.
            (Mode::Incomplete { view }, RecoveryState::Disabled) => {
                if view.is_idle() {
                    self.mode =
                        Mode::Default { view: DefaultRecoveryView::new(self.client.clone()) }
                }
            }

            (Mode::Incomplete { view }, RecoveryState::Enabled) => {
                if view.is_idle() {
                    self.mode =
                        Mode::Default { view: DefaultRecoveryView::new(self.client.clone()) }
                }
            }

            (Mode::Default { view }, RecoveryState::Incomplete) => {
                if view.is_idle() {
                    let view = RecoveringView::new(self.client.clone());
                    self.mode = Mode::Incomplete { view }
                }
            }

            // The recovery state didn't change in comparison to our desired view.
            (Mode::Incomplete { .. }, RecoveryState::Incomplete)
            | (Mode::Default { .. }, RecoveryState::Disabled | RecoveryState::Enabled)
            | (Mode::Unknown, RecoveryState::Unknown) => {}

            // The recovery state changed back to `Unknown`? This can never
            // happen but let's just go back to the `Unknown` view
            // showing a throbber.
            (Mode::Default { .. }, RecoveryState::Unknown)
            | (Mode::Incomplete { .. }, RecoveryState::Unknown) => {
                self.mode = Mode::Unknown;
            }
        }
    }

    pub async fn handle_key_press(&mut self, key: KeyEvent) -> bool {
        use KeyCode::*;

        match &mut self.mode {
            Mode::Unknown => matches!((key.modifiers, key.code), (_, Esc | Char('q'))),
            Mode::Incomplete { view } => match view.handle_key(key) {
                ShouldExit::No => false,
                ShouldExit::OnlySubScreen => {
                    self.mode = Mode::Unknown;
                    false
                }
                ShouldExit::Yes => true,
            },
            Mode::Default { view } => match view.handle_key(key).await {
                ShouldExit::No => false,
                ShouldExit::OnlySubScreen => {
                    self.mode = Mode::Unknown;
                    false
                }
                ShouldExit::Yes => true,
            },
        }
    }

    pub fn on_tick(&mut self) {
        self.throbber_state.calc_next();

        match &mut self.mode {
            Mode::Unknown => (),
            Mode::Incomplete { view } => view.on_tick(),
            Mode::Default { view } => view.on_tick(),
        }
    }

    fn get_throbber(&self, title: &'static str) -> Throbber<'static> {
        Throbber::default().label(title).throbber_set(throbber_widgets_tui::BRAILLE_EIGHT_DOUBLE)
    }
}

pub fn create_centered_throbber_area(area: Rect) -> Rect {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .margin(1)
        .constraints([Constraint::Fill(1), Constraint::Length(12), Constraint::Fill(1)])
        .split(area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([Constraint::Fill(1), Constraint::Length(1), Constraint::Fill(1)])
        .split(chunks[1]);

    chunks[1]
}

impl StatefulWidget for &mut RecoveryView {
    type State = RecoveryViewState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        state.update_state();

        // Let's now render our current screen.
        match &mut state.mode {
            Mode::Unknown => {
                let throbber = state.get_throbber("Loading");
                let centered_area = create_centered_throbber_area(area);
                StatefulWidget::render(throbber, centered_area, buf, &mut state.throbber_state);
            }
            Mode::Default { view } => {
                view.render(area, buf);
            }
            Mode::Incomplete { view } => {
                view.render(area, buf);
            }
        }
    }
}
