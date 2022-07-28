use std::{
    collections::btree_map::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use futures_signals::{signal::Mutable, signal_vec::MutableVec};
use log::warn;
use matrix_sdk::{
    ruma::{events::AnyRoomEvent, OwnedRoomId},
    Client, SlidingSyncRoom, SlidingSyncView,
};
use tuirealm::tui::widgets::TableState;

#[derive(Clone, Default)]
pub struct CurrentRoomSummary {
    pub name: String,
    pub state_events_counts: Vec<(String, usize)>,
    //pub state_events: BTreeMap<String, Vec<SlidingSyncRoom>>,
}

#[derive(Clone)]
pub struct SlidingSyncState {
    started: Instant,
    view: SlidingSyncView,
    /// the current list selector for the room
    first_render: Option<Duration>,
    full_sync: Option<Duration>,
    current_rooms_count: Option<u32>,
    total_rooms_count: Option<u32>,
    pub selected_room: Mutable<Option<OwnedRoomId>>,
}

impl std::cmp::PartialOrd for SlidingSyncState {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        None
    }
}

impl std::cmp::Ord for SlidingSyncState {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        std::cmp::Ordering::Equal
    }
}

impl std::cmp::Eq for SlidingSyncState {}

impl std::cmp::PartialEq for SlidingSyncState {
    fn eq(&self, other: &SlidingSyncState) -> bool {
        false
    }

    fn ne(&self, other: &SlidingSyncState) -> bool {
        false
    }
}

impl SlidingSyncState {
    pub fn new(view: SlidingSyncView) -> Self {
        Self {
            started: Instant::now(),
            view,
            first_render: None,
            full_sync: None,
            selected_room: Default::default(),
            current_rooms_count: None,
            total_rooms_count: None,
        }
    }

    pub fn started(&self) -> &Instant {
        &self.started
    }

    pub fn select_room(&self, r: Option<OwnedRoomId>) {
        self.selected_room.replace(r);
    }

    pub fn time_to_first_render(&self) -> Option<Duration> {
        self.first_render.clone()
    }

    pub fn time_to_full_sync(&self) -> Option<Duration> {
        self.full_sync.clone()
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
