// Copyright 2024 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{ops::Deref, sync::Arc};

use eyeball_im::{
    ObservableVector, ObservableVectorEntries, ObservableVectorEntry, ObservableVectorTransaction,
    ObservableVectorTransactionEntry, VectorSubscriber,
};
use imbl::Vector;

use super::TimelineItem;

#[derive(Debug)]
pub struct ObservableItems {
    items: ObservableVector<Arc<TimelineItem>>,
}

impl ObservableItems {
    pub fn new() -> Self {
        Self {
            // Upstream default capacity is currently 16, which is making
            // sliding-sync tests with 20 events lag. This should still be
            // small enough.
            items: ObservableVector::with_capacity(32),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn subscribe(&self) -> VectorSubscriber<Arc<TimelineItem>> {
        self.items.subscribe()
    }

    pub fn clone(&self) -> Vector<Arc<TimelineItem>> {
        self.items.clone()
    }

    pub fn transaction(&mut self) -> ObservableItemsTransaction<'_> {
        ObservableItemsTransaction { items: self.items.transaction() }
    }

    pub fn set(
        &mut self,
        timeline_item_index: usize,
        timeline_item: Arc<TimelineItem>,
    ) -> Arc<TimelineItem> {
        self.items.set(timeline_item_index, timeline_item)
    }

    pub fn entries(&mut self) -> ObservableVectorEntries<'_, Arc<TimelineItem>> {
        self.items.entries()
    }

    pub fn for_each<F>(&mut self, f: F)
    where
        F: FnMut(ObservableVectorEntry<'_, Arc<TimelineItem>>),
    {
        self.items.for_each(f)
    }
}

// It's fine to deref to an immutable reference to `Vector`.
impl Deref for ObservableItems {
    type Target = Vector<Arc<TimelineItem>>;

    fn deref(&self) -> &Self::Target {
        &self.items
    }
}

#[derive(Debug)]
pub struct ObservableItemsTransaction<'observable_items> {
    items: ObservableVectorTransaction<'observable_items, Arc<TimelineItem>>,
}

impl<'observable_items> ObservableItemsTransaction<'observable_items> {
    pub fn get(&self, timeline_item_index: usize) -> Option<&Arc<TimelineItem>> {
        self.items.get(timeline_item_index)
    }

    pub fn set(
        &mut self,
        timeline_item_index: usize,
        timeline_item: Arc<TimelineItem>,
    ) -> Arc<TimelineItem> {
        self.items.set(timeline_item_index, timeline_item)
    }

    pub fn remove(&mut self, timeline_item_index: usize) -> Arc<TimelineItem> {
        self.items.remove(timeline_item_index)
    }

    pub fn insert(&mut self, timeline_item_index: usize, timeline_item: Arc<TimelineItem>) {
        self.items.insert(timeline_item_index, timeline_item);
    }

    pub fn push_front(&mut self, timeline_item: Arc<TimelineItem>) {
        self.items.push_front(timeline_item);
    }

    pub fn push_back(&mut self, timeline_item: Arc<TimelineItem>) {
        self.items.push_back(timeline_item);
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    pub fn for_each<F>(&mut self, f: F)
    where
        F: FnMut(ObservableVectorTransactionEntry<'_, 'observable_items, Arc<TimelineItem>>),
    {
        self.items.for_each(f)
    }

    pub fn commit(self) {
        self.items.commit()
    }
}

// It's fine to deref to an immutable reference to `Vector`.
impl Deref for ObservableItemsTransaction<'_> {
    type Target = Vector<Arc<TimelineItem>>;

    fn deref(&self) -> &Self::Target {
        &self.items
    }
}
