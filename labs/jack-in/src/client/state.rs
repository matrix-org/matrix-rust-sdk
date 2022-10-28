use std::time::{Duration, Instant};

use futures_signals::signal::Mutable;
use matrix_sdk::{ruma::OwnedRoomId, SlidingSyncView};

#[derive(Clone, Default)]
pub struct CurrentRoomSummary {
    pub name: String,
    pub state_events_counts: Vec<(String, usize)>,
    //pub state_events: BTreeMap<String, Vec<SlidingSyncRoom>>,
}

#[derive(Clone, Debug)]
pub struct SlidingSyncState {
    started: Instant,
    view: SlidingSyncView,
    /// the current list selector for the room
    first_render: Option<Duration>,
    full_sync: Option<Duration>,
    pub selected_room: Mutable<Option<OwnedRoomId>>,
}

impl SlidingSyncState {
    pub fn new(view: SlidingSyncView) -> Self {
        Self {
            started: Instant::now(),
            view,
            first_render: None,
            full_sync: None,
            selected_room: Default::default(),
        }
    }

    pub fn started(&self) -> &Instant {
        &self.started
    }

    pub fn has_selected_room(&self) -> bool {
        self.selected_room.lock_ref().is_some()
    }

    pub fn select_room(&self, r: Option<OwnedRoomId>) {
        self.selected_room.replace(r);
    }

    pub fn time_to_first_render(&self) -> Option<Duration> {
        self.first_render
    }

    pub fn time_to_full_sync(&self) -> Option<Duration> {
        self.full_sync
    }

    pub fn loaded_rooms_count(&self) -> usize {
        self.view.rooms.lock_ref().len()
    }

    pub fn total_rooms_count(&self) -> Option<u32> {
        self.view.rooms_count.get()
    }

    pub fn set_first_render_now(&mut self) {
        self.first_render = Some(self.started.elapsed())
    }

    pub fn view(&self) -> &SlidingSyncView {
        &self.view
    }

    pub fn set_full_sync_now(&mut self) {
        self.full_sync = Some(self.started.elapsed())
    }
}
