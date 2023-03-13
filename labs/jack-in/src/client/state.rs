use std::{
    sync::{Arc, RwLock as StdRwLock},
    time::{Duration, Instant},
};

use eyeball::shared::Observable as SharedObservable;
use eyeball_im::{ObservableVector, VectorDiff};
use futures::{pin_mut, StreamExt};
use matrix_sdk::{
    room::timeline::{Timeline, TimelineItem},
    ruma::{OwnedRoomId, RoomId},
    SlidingSync, SlidingSyncList, SlidingSyncRoom, SlidingSyncState as ViewState,
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
    view: SlidingSyncList,
    /// the current list selector for the room
    first_render: Option<Duration>,
    full_sync: Option<Duration>,
    current_state: ViewState,
    tl_handle: SharedObservable<Option<JoinHandle<()>>>,
    pub selected_room: SharedObservable<Option<OwnedRoomId>>,
    pub current_timeline: Arc<StdRwLock<ObservableVector<Arc<TimelineItem>>>>,
    pub room_timeline: SharedObservable<Option<Timeline>>,
}

impl SlidingSyncState {
    pub fn new(syncer: SlidingSync, view: SlidingSyncList) -> Self {
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
        self.selected_room.read().is_some()
    }

    pub fn select_room(&self, r: Option<OwnedRoomId>) {
        self.current_timeline.write().unwrap().clear();
        if let Some(c) = self.tl_handle.take() {
            c.abort();
        }
        if let Some(room) = r.as_ref().and_then(|room_id| self.get_room(room_id)) {
            let current_timeline = self.current_timeline.clone();
            let room_timeline = self.room_timeline.clone();
            let handle = tokio::spawn(async move {
                let timeline = room.timeline().await.unwrap();
                let (items, listener) = timeline.subscribe().await;
                room_timeline.set(Some(timeline));
                {
                    let mut lock = current_timeline.write().unwrap();
                    lock.clear();
                    lock.append(items.into_iter().collect());
                }
                pin_mut!(listener);
                while let Some(diff) = listener.next().await {
                    match diff {
                        VectorDiff::Append { values } => {
                            current_timeline.write().unwrap().append(values);
                        }
                        VectorDiff::Clear => {
                            current_timeline.write().unwrap().clear();
                        }
                        VectorDiff::Insert { index, value } => {
                            current_timeline.write().unwrap().insert(index, value);
                        }
                        VectorDiff::PopBack => {
                            current_timeline.write().unwrap().pop_back();
                        }
                        VectorDiff::PopFront => {
                            current_timeline.write().unwrap().pop_front();
                        }
                        VectorDiff::PushBack { value } => {
                            current_timeline.write().unwrap().push_back(value);
                        }
                        VectorDiff::PushFront { value } => {
                            current_timeline.write().unwrap().push_front(value);
                        }
                        VectorDiff::Remove { index } => {
                            current_timeline.write().unwrap().remove(index);
                        }
                        VectorDiff::Set { index, value } => {
                            current_timeline.write().unwrap().set(index, value);
                        }
                        VectorDiff::Reset { values } => {
                            let mut lock = current_timeline.write().unwrap();
                            lock.clear();
                            lock.append(values);
                        }
                    }
                }
            });
            self.tl_handle.set(Some(handle));
        }
        self.selected_room.set(r);
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

    pub fn view(&self) -> &SlidingSyncList {
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
