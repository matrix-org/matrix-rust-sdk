use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use assign::assign;
use futures_signals::{
    signal::SignalExt,
    signal_vec::{SignalVecExt, VecDiff},
};
use futures_util::{pin_mut, StreamExt};
pub use matrix_sdk::{RoomListEntry as MatrixRoomEntry, SlidingSyncState};
use parking_lot::RwLock;
use matrix_sdk::ruma::api::client::sync::sliding_sync_events::RoomSubscription as RumaRoomSubscription;

use super::{Client, RUNTIME};

pub struct StoppableSpawn {
    cancelled: AtomicBool,
}

impl StoppableSpawn {
    fn new() -> StoppableSpawn {
        StoppableSpawn { cancelled: AtomicBool::new(false) }
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed)
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

pub struct UpdateSummary {
    /// The views (according to their name), which have seen an update
    pub views: Vec<String>,
    pub rooms: Vec<String>,
}

pub struct RoomSubscriptionRequiredState {
    pub key: String,
    pub value: String,
}

pub struct RoomSubscription {
    pub required_state: Option<Vec<RoomSubscriptionRequiredState>>,
    pub timeline_limit: Option<u32>,
}

impl TryInto<RumaRoomSubscription> for RoomSubscription {
    type Error = anyhow::Error;
    fn try_into(self) -> anyhow::Result<RumaRoomSubscription> {
        Ok(assign!(RumaRoomSubscription::default(), {
            required_state: self.required_state.map(|r|
                r.into_iter().map(|s| (s.key.into(), s.value)).collect()
            ),
            timeline_limit: self.timeline_limit.map(|u| u.into())
        }))
    }
}

impl From<matrix_sdk::UpdateSummary> for UpdateSummary {
    fn from(other: matrix_sdk::UpdateSummary) -> UpdateSummary {
        UpdateSummary {
            views: other.views,
            rooms: other.rooms.into_iter().map(|r| r.as_str().to_owned()).collect(),
        }
    }
}

pub enum SlidingSyncViewRoomsListDiff {
    Replace { values: Vec<RoomListEntry> },
    InsertAt { index: u32, value: RoomListEntry },
    UpdateAt { index: u32, value: RoomListEntry },
    RemoveAt { index: u32 },
    Move { old_index: u32, new_index: u32 },
    Push { value: RoomListEntry },
}

impl From<VecDiff<MatrixRoomEntry>> for SlidingSyncViewRoomsListDiff {
    fn from(other: VecDiff<MatrixRoomEntry>) -> Self {
        match other {
            VecDiff::Replace { values } => SlidingSyncViewRoomsListDiff::Replace {
                values: values.into_iter().map(|e| (&e).into()).collect(),
            },
            VecDiff::InsertAt { index, value } => SlidingSyncViewRoomsListDiff::InsertAt {
                index: index as u32,
                value: (&value).into(),
            },
            VecDiff::UpdateAt { index, value } => SlidingSyncViewRoomsListDiff::UpdateAt {
                index: index as u32,
                value: (&value).into(),
            },
            VecDiff::RemoveAt { index } => {
                SlidingSyncViewRoomsListDiff::RemoveAt { index: index as u32 }
            }
            VecDiff::Move { old_index, new_index } => SlidingSyncViewRoomsListDiff::Move {
                old_index: old_index as u32,
                new_index: new_index as u32,
            },
            VecDiff::Push { value } => {
                SlidingSyncViewRoomsListDiff::Push { value: (&value).into() }
            }
            _ => unimplemented!("Clear and Pop aren't provided within sliding sync"),
        }
    }
}

#[derive(Clone, Debug)]
pub enum RoomListEntry {
    Empty,
    Invalidated { room_id: String },
    Filled { room_id: String },
}

impl From<&MatrixRoomEntry> for RoomListEntry {
    fn from(other: &MatrixRoomEntry) -> Self {
        match other {
            MatrixRoomEntry::Empty => RoomListEntry::Empty,
            MatrixRoomEntry::Filled(b) => RoomListEntry::Filled { room_id: b.to_string() },
            MatrixRoomEntry::Invalidated(b) => {
                RoomListEntry::Invalidated { room_id: b.to_string() }
            }
        }
    }
}

pub trait SlidingSyncViewRoomsListDelegate: Sync + Send {
    fn did_receive_update(&self, diff: SlidingSyncViewRoomsListDiff);
}

pub trait SlidingSyncViewRoomsCountDelegate: Sync + Send {
    fn did_receive_update(&self, new_count: u32);
}

pub trait SlidingSyncViewStateDelegate: Sync + Send {
    fn did_receive_update(&self, new_state: SlidingSyncState);
}

pub struct SlidingSyncView {
    inner: matrix_sdk::SlidingSyncView,
}

impl SlidingSyncView {
    pub fn on_state_update(
        &self,
        delegate: Box<dyn SlidingSyncViewStateDelegate>,
    ) -> Arc<StoppableSpawn> {
        let outer_stopper = Arc::new(StoppableSpawn::new());
        let stopper = outer_stopper.clone();
        let mut signal = self.inner.state.signal_cloned().to_stream();
        RUNTIME.spawn(async move {
            loop {
                let update = signal.next().await;
                if stopper.is_cancelled() {
                    break;
                }
                if let Some(new_state) = update {
                    delegate.did_receive_update(new_state);
                }
            }
        });
        outer_stopper
    }

    pub fn on_rooms_update(
        &self,
        delegate: Box<dyn SlidingSyncViewRoomsListDelegate>,
    ) -> Arc<StoppableSpawn> {
        let outer_stopper = Arc::new(StoppableSpawn::new());
        let stopper = outer_stopper.clone();
        let mut room_list = self.inner.rooms_list.signal_vec_cloned().to_stream();
        RUNTIME.spawn(async move {
            loop {
                let list_diff = room_list.next().await;
                if stopper.is_cancelled() {
                    break;
                }
                if let Some(diff) = list_diff {
                    delegate.did_receive_update(diff.into());
                }
            }
        });
        outer_stopper
    }

    pub fn on_rooms_count_update(
        &self,
        delegate: Box<dyn SlidingSyncViewRoomsCountDelegate>,
    ) -> Arc<StoppableSpawn> {
        let outer_stopper = Arc::new(StoppableSpawn::new());
        let stopper = outer_stopper.clone();
        let mut rooms_count = self.inner.rooms_count.signal_cloned().to_stream();
        RUNTIME.spawn(async move {
            loop {
                let new_count = rooms_count.next().await;
                if stopper.is_cancelled() {
                    break;
                }
                if let Some(Some(new)) = new_count {
                    delegate.did_receive_update(new);
                }
            }
        });
        outer_stopper
    }

    pub fn current_room_count(&self) -> Option<u32> {
        self.inner.rooms_count.get_cloned()
    }
    pub fn current_rooms_list(&self) -> Vec<RoomListEntry> {
        self.inner.rooms_list.lock_ref().as_slice().into_iter().map(|e| e.into()).collect()
    }
}

pub trait SlidingSyncDelegate: Sync + Send {
    fn did_receive_sync_update(&self, summary: UpdateSummary);
}

pub struct SlidingSync {
    inner: matrix_sdk::SlidingSync,
    delegate: Arc<RwLock<Option<Box<dyn SlidingSyncDelegate>>>>,
}

impl SlidingSync {
    pub fn on_update(&self, delegate: Option<Box<dyn SlidingSyncDelegate>>) {
        *self.delegate.write() = delegate;
    }

    pub fn subscribe(
        &self,
        room_id: String,
        settings: Option<RoomSubscription>,
    ) -> anyhow::Result<()> {
        let settings =
            if let Some(settings) = settings { Some(settings.try_into()?) } else { None };
        self.inner.subscribe(room_id.try_into()?, settings);
        Ok(())
    }

    pub fn unsubscribe(&self, room_id: String) -> anyhow::Result<()> {
        self.inner.unsubscribe(room_id.try_into()?);
        Ok(())
    }

    pub fn get_view(&self, name: String) -> Option<Arc<SlidingSyncView>> {
        for s in self.inner.views.lock_ref().iter() {
            if s.name == name {
                return Some(Arc::new(SlidingSyncView { inner: s.clone() }));
            }
        }
        None
    }

    pub fn start_sync(&self) {
        let inner = self.inner.clone();
        let delegate = self.delegate.clone();

        RUNTIME.spawn(async move {
            let (_cancel, stream) = inner.stream().await.expect("Doesn't fail.");
            pin_mut!(stream);
            for update in stream.next().await {
                let update = match update {
                    Ok(u) => u,
                    Err(e) => {
                        // FIXME: send this over the FFI
                        println!("Sliding Sync failure: {:?}", e);
                        continue;
                    }
                };
                if let Some(ref delegate) = *delegate.read() {
                    delegate.did_receive_sync_update(update.into());
                } else {
                    // when the delegate has been removed
                    // we cancel the loop
                    break;
                }
            }
        });
    }
}

impl Client {
    pub fn full_sliding_sync(&self) -> anyhow::Result<Arc<SlidingSync>> {
        let mut builder = self.client.sliding_sync();
        let inner = builder.add_fullsync_view().build()?;
        Ok(Arc::new(SlidingSync { inner, delegate: Arc::new(RwLock::new(None)) }))
    }
}
