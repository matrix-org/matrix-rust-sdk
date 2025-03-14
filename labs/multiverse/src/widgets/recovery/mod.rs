use crossterm::event::{KeyCode, KeyEvent};
use layout::Flex;
use matrix_sdk::{encryption::recovery::RecoveryState, Client};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Padding},
};
use recovering::RecoveringView;
use throbber_widgets_tui::{Throbber, ThrobberState};

mod disabled;
mod recovering;

use disabled::DisabledView;

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
    Disabled {
        view: DisabledView,
    },
}

impl PartialEq for RecoveryViewState {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

pub enum ShouldExit {
    No,
    OnlySubScreen,
    Yes,
}

impl Eq for RecoveryViewState {}

impl RecoveryViewState {
    pub fn new(client: Client) -> Self {
        Self { client, throbber_state: ThrobberState::default(), mode: Mode::default() }
    }

    fn update_state(&mut self) {
        let recovery_state = self.client.encryption().recovery().state();

        match (&mut self.mode, recovery_state) {
            (Mode::Unknown, RecoveryState::Disabled) => {
                let view = RecoveringView::new(self.client.clone());
                self.mode = Mode::Incomplete { view }

                // self.mode = Mode::Disabled { view:
                // DisabledView::new(self.client.clone()) };
            }

            // The recovery state changed to incomplete, we go into the incomplete view.
            (Mode::Unknown, RecoveryState::Incomplete) => {
                let view = RecoveringView::new(self.client.clone());
                self.mode = Mode::Incomplete { view }
            }

            // TODO: add an Unknown -> Enabled transition.
            _ => {}
        }
    }

    pub fn handle_key_press(&mut self, key: KeyEvent) -> bool {
        use KeyCode::*;

        match &mut self.mode {
            Mode::Unknown => match (key.modifiers, key.code) {
                (_, Esc) => true,
                _ => false,
            },
            Mode::Incomplete { view } => match view.handle_key(key) {
                ShouldExit::No => false,
                ShouldExit::OnlySubScreen => {
                    self.mode = Mode::Unknown;
                    false
                }
                ShouldExit::Yes => true,
            },
            Mode::Disabled { view } => match view.handle_key(key) {
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
            Mode::Disabled { view } => view.on_tick(),
        }
    }

    fn get_throbber(&self, title: &'static str) -> Throbber<'static> {
        Throbber::default().label(title).throbber_set(throbber_widgets_tui::BRAILLE_EIGHT_DOUBLE)
    }
}

fn create_centered_throbber_area<'a>(area: Rect) -> Rect {
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
        // Create a centered popout with 8 lines and 70 rows.
        let vertical =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(10), Constraint::Fill(1)])
                .flex(Flex::Center);

        let horizontal =
            Layout::horizontal([Constraint::Fill(1), Constraint::Min(50), Constraint::Fill(1)])
                .flex(Flex::Center);

        let [_, area, _] = vertical.areas(area);
        let [_, area, _] = horizontal.areas(area);

        // Clear that part of the screen so we can draw our bounding block and other
        // widgets inside the block.
        Clear.render(area, buf);

        // Render our block, mainly for the border.
        let block = Block::bordered()
            .title(Line::from("Encryption").centered())
            .borders(Borders::ALL)
            .padding(Padding::left(2));
        block.render(area, buf);

        // The block uses borders so let's add margins so new widgets don't draw over
        // the block.
        let [usable_area] = Layout::vertical([Constraint::Fill(1)])
            .horizontal_margin(2)
            .vertical_margin(1)
            .areas(area);

        // Let's first see if our state has changed.
        // TODO: Should we do this in our `on_tick()` method?
        state.update_state();

        // Let's now render our current screen.
        match &mut state.mode {
            Mode::Unknown => {
                let throbber = state.get_throbber("Loading");
                let centered_area = create_centered_throbber_area(usable_area);
                StatefulWidget::render(throbber, centered_area, buf, &mut state.throbber_state);
            }
            Mode::Disabled { view } => {
                view.render(usable_area, buf);
            }
            Mode::Incomplete { view } => {
                view.render(usable_area, buf);
            }
        }
    }
}
