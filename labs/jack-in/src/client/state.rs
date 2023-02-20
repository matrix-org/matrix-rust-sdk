use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use eyeball_im::VectorDiff;
use futures::{pin_mut, StreamExt};
use futures_signals::{signal::Mutable, signal_vec::MutableVec};
use matrix_sdk::{
    room::timeline::{Timeline, TimelineItem},
    ruma::{OwnedRoomId, RoomId},
    SlidingSync, SlidingSyncRoom, SlidingSyncState as ViewState, SlidingSyncView,
};
use tokio::task::JoinHandle;

#[derive(Clone, Default)]
pub struct CurrentRoomSummary {
    pub name: String,
    pub state_events_counts: Vec<(String, usize)>,
    //pub state_events: BTreeMap<String, Vec<SlidingSyncRoom>>,
}

#[derive(Clone, Debug)]
pub struct SlidingSyncState {
    started: Instant,
    syncer: SlidingSync,
    view: SlidingSyncView,
    /// the current list selector for the room
    first_render: Option<Duration>,
    full_sync: Option<Duration>,
    current_state: ViewState,
    tl_handle: Mutable<Option<JoinHandle<()>>>,
    pub selected_room: Mutable<Option<OwnedRoomId>>,
    pub current_timeline: MutableVec<Arc<TimelineItem>>,
    pub room_timeline: Mutable<Option<Timeline>>,
}

impl SlidingSyncState {
    pub fn new(syncer: SlidingSync, view: SlidingSyncView) -> Self {
        Self {
            started: Instant::now(),
            syncer,
            view,
            first_render: None,
            full_sync: None,
            current_state: ViewState::default(),
            tl_handle: Default::default(),
            selected_room: Default::default(),
            current_timeline: Default::default(),
            room_timeline: Default::default(),
        }
    }

    pub fn started(&self) -> &Instant {
        &self.started
    }

    pub fn has_selected_room(&self) -> bool {
        self.selected_room.lock_ref().is_some()
    }

    pub fn select_room(&self, r: Option<OwnedRoomId>) {
        self.current_timeline.lock_mut().clear();
        if let Some(c) = self.tl_handle.lock_mut().take() {
            c.abort();
        }
        if let Some(room) = r.as_ref().and_then(|room_id| self.get_room(room_id)) {
            let current_timeline = self.current_timeline.clone();
            let room_timeline = self.room_timeline.clone();
            let handle = tokio::spawn(async move {
                let timeline = room.timeline().await.unwrap();
                let (items, listener) = timeline.subscribe().await;
                *room_timeline.lock_mut() = Some(timeline);
                current_timeline.lock_mut().replace_cloned(items.into_iter().collect());
                pin_mut!(listener);
                while let Some(diff) = listener.next().await {
                    match diff {
                        VectorDiff::Append { values } => {
                            let mut lock = current_timeline.lock_mut();
                            for v in values {
                                lock.push_cloned(v);
                            }
                        }
                        VectorDiff::Clear => {
                            current_timeline.lock_mut().clear();
                        }
                        VectorDiff::Insert { index, value } => {
                            current_timeline.lock_mut().insert_cloned(index, value);
                        }
                        VectorDiff::PopBack => {
                            current_timeline.lock_mut().pop();
                        }
                        VectorDiff::PopFront => {
                            current_timeline.lock_mut().remove(0);
                        }
                        VectorDiff::PushBack { value } => {
                            current_timeline.lock_mut().push_cloned(value);
                        }
                        VectorDiff::PushFront { value } => {
                            current_timeline.lock_mut().insert_cloned(0, value);
                        }
                        VectorDiff::Remove { index } => {
                            current_timeline.lock_mut().remove(index);
                        }
                        VectorDiff::Set { index, value } => {
                            current_timeline.lock_mut().set_cloned(index, value);
                        }
                        VectorDiff::Reset { values } => {
                            let mut lock = current_timeline.lock_mut();
                            lock.clear();
                            for v in values {
                                lock.push_cloned(v);
                            }
                        }
                    }
                }
            });
            *self.tl_handle.lock_mut() = Some(handle);
        }
        self.selected_room.replace(r);
    }

    pub fn time_to_first_render(&self) -> Option<Duration> {
        self.first_render
    }

    pub fn time_to_full_sync(&self) -> Option<Duration> {
        self.full_sync
    }
    pub fn current_state(&self) -> &ViewState {
        &self.current_state
    }

    pub fn loaded_rooms_count(&self) -> usize {
        self.syncer.get_number_of_rooms()
    }

    pub fn total_rooms_count(&self) -> Option<u32> {
        self.view.rooms_count()
    }

    pub fn set_first_render_now(&mut self) {
        self.first_render = Some(self.started.elapsed())
    }

    pub fn view(&self) -> &SlidingSyncView {
        &self.view
    }

    pub fn get_room(&self, room_id: &RoomId) -> Option<SlidingSyncRoom> {
        self.syncer.get_room(room_id)
    }

    pub fn get_all_rooms(&self) -> Vec<SlidingSyncRoom> {
        self.syncer.get_all_rooms()
    }

    pub fn set_full_sync_now(&mut self) {
        self.full_sync = Some(self.started.elapsed())
    }

    pub fn set_view_state(&mut self, current_state: ViewState) {
        self.current_state = current_state
    }
}
