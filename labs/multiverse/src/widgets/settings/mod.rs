use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use developer::DeveloperSettingsView;
use matrix_sdk::Client;
use matrix_sdk_ui::sync_service::SyncService;
use ratatui::{prelude::*, widgets::*};
use strum::{Display, EnumIter, FromRepr, IntoEnumIterator};
use style::palette::tailwind;

use super::recovery::{RecoveryView, RecoveryViewState};
use crate::popup_area;

mod developer;

// TODO: This replicates a lot of the logic the details view has, we should make
// a generic tab popout widget to share a bit of logic here.

#[derive(Clone, Copy, Default, Display, FromRepr, EnumIter)]
enum SelectedTab {
    /// Show the developer settings we have.
    #[default]
    Developer,

    /// Show the encryption settings
    Encryption,
}

impl SelectedTab {
    /// Get the previous tab, if there is no previous tab return the current
    /// tab.
    fn previous(self) -> Self {
        let current_index: usize = self as usize;
        let previous_index = current_index.saturating_sub(1);
        Self::from_repr(previous_index).unwrap_or(self)
    }

    /// Get the next tab, if there is no next tab return the current tab.
    fn next(self) -> Self {
        let current_index = self as usize;
        let next_index = current_index.saturating_add(1);
        Self::from_repr(next_index).unwrap_or(self)
    }

    /// Cycle to the next tab, if we're at the last tab we return the first and
    /// default tab.
    fn cycle_next(self) -> Self {
        let current_index = self as usize;
        let next_index = current_index.saturating_add(1);
        Self::from_repr(next_index).unwrap_or_default()
    }

    /// Cycle to the previous tab, if we're at the first tab we return the last
    /// tab.
    fn cycle_prev(self) -> Self {
        let current_index = self as usize;

        if current_index == 0 {
            Self::iter().next_back().expect("We should always have a last element in our enum")
        } else {
            let previous_index = current_index.saturating_sub(1);
            Self::from_repr(previous_index).unwrap_or(self)
        }
    }

    /// Return tab's name as a styled `Line`
    fn title(self) -> Line<'static> {
        format!("  {self}  ").fg(tailwind::SLATE.c200).bg(self.palette().c900).into()
    }

    const fn palette(&self) -> tailwind::Palette {
        match self {
            Self::Developer => tailwind::BLUE,
            Self::Encryption => tailwind::EMERALD,
        }
    }
}

pub struct SettingsView {
    selected_tab: SelectedTab,

    developer_settings_view: DeveloperSettingsView,
    recovery_view_state: RecoveryViewState,
}

impl SettingsView {
    pub fn new(client: Client, sync_service: Arc<SyncService>) -> Self {
        let recovery_view_state = RecoveryViewState::new(client.clone());
        let developer_settings_view = DeveloperSettingsView::new(client, sync_service);

        Self { selected_tab: SelectedTab::default(), recovery_view_state, developer_settings_view }
    }

    pub async fn handle_key_press(&mut self, event: KeyEvent) -> bool {
        use KeyCode::*;

        match event.code {
            Right => {
                self.next_tab();
                false
            }

            Tab => {
                self.cycle_next_tab();
                false
            }

            BackTab => {
                self.cycle_prev_tab();
                false
            }

            Left => {
                self.previous_tab();
                false
            }

            Char('q') | Esc => match self.selected_tab {
                SelectedTab::Developer => true,
                SelectedTab::Encryption => self.recovery_view_state.handle_key_press(event).await,
            },

            _ => match self.selected_tab {
                SelectedTab::Developer => {
                    self.developer_settings_view.handle_key_press(event).await;
                    false
                }
                SelectedTab::Encryption => self.recovery_view_state.handle_key_press(event).await,
            },
        }
    }

    pub fn on_tick(&mut self) {
        self.recovery_view_state.on_tick();
    }

    fn cycle_next_tab(&mut self) {
        self.selected_tab = self.selected_tab.cycle_next();
    }

    fn cycle_prev_tab(&mut self) {
        self.selected_tab = self.selected_tab.cycle_prev();
    }

    fn next_tab(&mut self) {
        self.selected_tab = self.selected_tab.next();
    }

    fn previous_tab(&mut self) {
        self.selected_tab = self.selected_tab.previous();
    }
}

impl Widget for &mut SettingsView {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        use Constraint::{Length, Min};

        let area = popup_area(area, 40, 30);
        Clear.render(area, buf);

        let vertical = Layout::vertical([Length(1), Min(0), Length(1)]);
        let [header_area, inner_area, footer_area] = vertical.areas(area);

        let horizontal = Layout::horizontal([Min(0), Length(20)]);
        let [tab_title_area, title_area] = horizontal.areas(header_area);

        "Settings".bold().render(title_area, buf);

        Block::bordered()
            .border_set(symbols::border::PROPORTIONAL_TALL)
            .padding(Padding::horizontal(1))
            .border_style(tailwind::BLUE.c700)
            .render(inner_area, buf);

        let titles = SelectedTab::iter().map(SelectedTab::title);
        let highlight_style = (Color::default(), self.selected_tab.palette().c700);
        let selected_tab_index = self.selected_tab as usize;

        let tabs_area = inner_area.inner(Margin::new(1, 1));

        Tabs::new(titles)
            .highlight_style(highlight_style)
            .select(selected_tab_index)
            .padding("", "")
            .divider(" ")
            .render(tab_title_area, buf);

        match self.selected_tab {
            SelectedTab::Developer => {
                self.developer_settings_view.render(tabs_area, buf);
            }
            SelectedTab::Encryption => {
                let mut view = RecoveryView::new();
                view.render(tabs_area, buf, &mut self.recovery_view_state);
            }
        }

        Line::raw("◄ ► to change tab | Press q to exit the settings screen")
            .centered()
            .render(footer_area, buf);
    }
}
