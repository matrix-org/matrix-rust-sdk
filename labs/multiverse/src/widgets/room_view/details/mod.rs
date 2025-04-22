use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use matrix_sdk_ui::room_list_service::Room;
use ratatui::{prelude::*, widgets::*};
use strum::{Display, EnumIter, FromRepr, IntoEnumIterator};
use style::palette::tailwind;
use threads::ThreadListLoader;

use self::{
    events::EventsView, linked_chunk::LinkedChunkView, read_receipts::ReadReceipts,
    threads::ThreadsView,
};
use crate::widgets::recovery::ShouldExit;

mod events;
mod linked_chunk;
mod read_receipts;
mod threads;

struct SelectedTabState {
    /// Which tab is currently selected.
    selected_tab: SelectedTab,

    kind: SelectedTabStateKind,
}

impl Default for SelectedTabState {
    fn default() -> Self {
        Self { selected_tab: SelectedTab::Events, kind: SelectedTabStateKind::Empty }
    }
}

#[derive(Default)]
enum SelectedTabStateKind {
    #[default]
    Empty,

    Threads(ThreadListLoader),
}

#[derive(Clone, Copy, Display, FromRepr, EnumIter)]
enum SelectedTab {
    /// Show the raw event sources of the timeline.
    Events,

    /// Show details about read receipts of the room.
    ReadReceipts,

    /// Show the linked chunks that are used to display the timeline.
    LinkedChunks,

    /// Show the list of threads in this room.
    Threads,
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
        Self::from_repr(next_index).unwrap_or(SelectedTab::Events)
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
            Self::Events => tailwind::BLUE,
            Self::ReadReceipts => tailwind::EMERALD,
            Self::LinkedChunks => tailwind::INDIGO,
            Self::Threads => tailwind::PURPLE,
        }
    }
}

impl<'a> StatefulWidget for &'a mut SelectedTabState {
    type State = Option<&'a Room>;

    fn render(self, area: Rect, buf: &mut Buffer, room: &mut Self::State)
    where
        Self: Sized,
    {
        match self.selected_tab {
            SelectedTab::Events => {
                EventsView::new(room.as_deref()).render(area, buf);
            }
            SelectedTab::ReadReceipts => {
                ReadReceipts::new(room.as_deref()).render(area, buf);
            }
            SelectedTab::LinkedChunks => LinkedChunkView::new(room.as_deref()).render(area, buf),
            SelectedTab::Threads => {
                let loader = match &mut self.kind {
                    SelectedTabStateKind::Empty => panic!("unexpected state for the threads view"),
                    SelectedTabStateKind::Threads(loader) => loader,
                };
                loader.init_if_needed(*room);
                ThreadsView::new().render(area, buf, loader)
            }
        }
    }
}

#[derive(Default)]
pub struct RoomDetails {
    state: SelectedTabState,
}

impl RoomDetails {
    /// Create a new [`RoomDetails`] struct with the [`SelectedTab::Events`] as
    /// the selected tab.
    pub fn with_events_as_selected() -> Self {
        Self {
            state: SelectedTabState {
                selected_tab: SelectedTab::Events,
                kind: SelectedTabStateKind::Empty,
            },
        }
    }

    /// Create a new [`RoomDetails`] struct with the
    /// [`SelectedTab::ReadReceipts`] as the selected tab.
    pub fn with_receipts_as_selected() -> Self {
        Self {
            state: SelectedTabState {
                selected_tab: SelectedTab::ReadReceipts,
                kind: SelectedTabStateKind::Empty,
            },
        }
    }

    /// Create a new [`RoomDetails`] struct with the
    /// [`SelectedTab::LinkedChunks`] as the selected tab.
    pub fn with_chunks_as_selected() -> Self {
        Self {
            state: SelectedTabState {
                selected_tab: SelectedTab::LinkedChunks,
                kind: SelectedTabStateKind::Empty,
            },
        }
    }

    /// Create a new [`RoomDetails`] struct with the
    /// [`SelectedTab::Threads`] as the selected tab.
    pub fn with_threads_as_selected() -> Self {
        Self {
            state: SelectedTabState {
                selected_tab: SelectedTab::Threads,
                kind: SelectedTabStateKind::Threads(ThreadListLoader::default()),
            },
        }
    }

    pub fn handle_key_press(&mut self, event: KeyEvent) -> ShouldExit {
        use KeyCode::*;

        if event.kind != KeyEventKind::Press {
            return ShouldExit::No;
        }

        match event.code {
            Char('l') | Right => {
                self.next_tab();
                ShouldExit::No
            }

            Tab => {
                self.cycle_next_tab();
                ShouldExit::No
            }

            BackTab => {
                self.cycle_prev_tab();
                ShouldExit::No
            }

            Char('h') | Left => {
                self.previous_tab();
                ShouldExit::No
            }

            Char('q') | Esc => ShouldExit::Yes,

            _ => ShouldExit::No,
        }
    }

    fn sync_state(&mut self) {
        match self.state.selected_tab {
            SelectedTab::Events | SelectedTab::ReadReceipts | SelectedTab::LinkedChunks => {
                self.state.kind = SelectedTabStateKind::Empty;
            }
            SelectedTab::Threads => {
                self.state.kind = SelectedTabStateKind::Threads(ThreadListLoader::default());
            }
        }
    }

    fn cycle_next_tab(&mut self) {
        self.state.selected_tab = self.state.selected_tab.cycle_next();
        self.sync_state();
    }

    fn cycle_prev_tab(&mut self) {
        self.state.selected_tab = self.state.selected_tab.cycle_prev();
        self.sync_state();
    }

    fn next_tab(&mut self) {
        self.state.selected_tab = self.state.selected_tab.next();
        self.sync_state();
    }

    fn previous_tab(&mut self) {
        self.state.selected_tab = self.state.selected_tab.previous();
        self.sync_state();
    }
}

impl<'a> StatefulWidget for &'a mut RoomDetails {
    type State = Option<&'a Room>;

    fn render(self, area: Rect, buf: &mut Buffer, room: &mut Self::State)
    where
        Self: Sized,
    {
        use Constraint::{Length, Min};
        let vertical = Layout::vertical([Length(1), Min(0), Length(1)]);
        let [header_area, inner_area, footer_area] = vertical.areas(area);

        let horizontal = Layout::horizontal([Min(0), Length(20)]);
        let [tab_title_area, title_area] = horizontal.areas(header_area);

        "Room details".bold().render(title_area, buf);

        Block::bordered()
            .border_set(symbols::border::PROPORTIONAL_TALL)
            .padding(Padding::horizontal(1))
            .border_style(tailwind::BLUE.c700)
            .render(inner_area, buf);

        let titles = SelectedTab::iter().map(SelectedTab::title);
        let highlight_style = (Color::default(), self.state.selected_tab.palette().c700);
        let selected_tab_index = self.state.selected_tab as usize;

        let tabs_area = inner_area.inner(Margin::new(1, 1));

        Tabs::new(titles)
            .highlight_style(highlight_style)
            .select(selected_tab_index)
            .padding("", "")
            .divider(" ")
            .render(tab_title_area, buf);

        self.state.render(tabs_area, buf, room);

        Line::raw("◄ ► to change tab | Press q to exit the details screen")
            .centered()
            .render(footer_area, buf);
    }
}
