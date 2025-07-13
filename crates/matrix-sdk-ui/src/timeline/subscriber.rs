// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use eyeball::Subscriber;
use eyeball_im::{VectorDiff, VectorSubscriberBatchedStream};
use eyeball_im_util::vector::{Skip, VectorObserverExt};
use futures_core::Stream;
use imbl::Vector;
use pin_project_lite::pin_project;

use super::{TimelineDropHandle, controller::ObservableItems, item::TimelineItem};

pin_project! {
    /// A stream that wraps a [`TimelineDropHandle`] so that the `Timeline`
    /// isn't dropped until the `Stream` is dropped.
    pub(super) struct TimelineWithDropHandle<S> {
        #[pin]
        inner: S,
        drop_handle: Arc<TimelineDropHandle>,
    }
}

impl<S> TimelineWithDropHandle<S> {
    /// Create a new [`WithTimelineDropHandle`].
    pub(super) fn new(inner: S, drop_handle: Arc<TimelineDropHandle>) -> Self {
        Self { inner, drop_handle }
    }
}

impl<S> Stream for TimelineWithDropHandle<S>
where
    S: Stream,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project().inner.poll_next(context)
    }
}

pin_project! {
    /// A type that creates a proper `Timeline` subscriber.
    ///
    /// This type implements [`Stream`], so that it's entirely transparent for
    /// all consumers expecting an `impl Stream`.
    ///
    /// This `Stream` pipes `VectorDiff`s from [`ObservableItems`] into a batched
    /// stream ([`VectorSubscriberBatchedStream`]), and then applies a skip
    /// higher-order stream ([`Skip`]).
    ///
    /// `Skip` works by skipping the first _n_ values, where _n_ is referred
    /// as `count`. Here, this `count` value is defined by a `Stream<Item =
    /// usize>` (see [`Skip::dynamic_skip_with_initial_count`]). Everytime
    /// the `count` stream produces a value, `Skip` adjusts its output.
    /// `count` is managed by [`SkipCount`][skip::SkipCount], and is hold in
    /// `TimelineMetadata::subscriber_skip_count`.
    pub(super) struct TimelineSubscriber {
        #[pin]
        inner: Skip<VectorSubscriberBatchedStream<Arc<TimelineItem>>, Subscriber<usize>>,
    }
}

impl TimelineSubscriber {
    /// Creates a [`TimelineSubscriber`], in addition to the initial values of
    /// the subscriber.
    pub(super) fn new(
        observable_items: &ObservableItems,
        observable_skip_count: &skip::SkipCount,
    ) -> (Vector<Arc<TimelineItem>>, Self) {
        let (initial_values, stream) = observable_items
            .subscribe()
            .into_values_and_batched_stream()
            .dynamic_skip_with_initial_count(
                // The `SkipCount` value may have been modified before the subscriber is
                // created. Let's use the current value instead of hardcoding it to 0.
                observable_skip_count.get(),
                observable_skip_count.subscribe(),
            );

        (initial_values, Self { inner: stream })
    }
}

impl Stream for TimelineSubscriber {
    type Item = Vec<VectorDiff<Arc<TimelineItem>>>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project().inner.poll_next(context)
    }
}

pub mod skip {
    use eyeball::{SharedObservable, Subscriber};

    const MAXIMUM_NUMBER_OF_INITIAL_ITEMS: usize = 20;

    /// `SkipCount` helps to manage the `count` value used by the [`Skip`]
    /// higher-order stream used by the [`TimelineSubscriber`]. See its
    /// documentation to learn more.
    ///
    /// [`Skip`]: eyeball_im_util::vector::Skip
    /// [`TimelineSubscriber`]: super::TimelineSubscriber
    #[derive(Clone, Debug)]
    pub struct SkipCount {
        count: SharedObservable<usize>,
    }

    impl SkipCount {
        /// Create a [`SkipCount`] with a default `count` value set to 0.
        pub fn new() -> Self {
            Self { count: SharedObservable::new(0) }
        }

        /// Compute the `count` value for [the `Skip` higher-order
        /// stream][`Skip`].
        ///
        /// This is useful when new items are inserted, removed and so on.
        ///
        /// [`Skip`]: eyeball_im_util::vector::Skip
        pub fn compute_next(
            &self,
            previous_number_of_items: usize,
            next_number_of_items: usize,
        ) -> usize {
            let current_count = self.count.get();

            // Initial states: no items are present.
            if previous_number_of_items == 0 {
                // Adjust the count to provide a maximum number of initial items. We want to
                // skip the first items until we get a certain number of items to display.
                //
                // | `next_number_of_items` | `MAX…` | output | will display |
                // |------------------------|--------|--------|--------------|
                // | 60                     | 20     | 40     | 20 items     |
                // | 10                     | 20     | 0      | 10 items     |
                // | 0                      | 20     | 0      | 0 item       |
                //
                next_number_of_items.saturating_sub(MAXIMUM_NUMBER_OF_INITIAL_ITEMS)
            }
            // Not the initial state: there are items.
            else {
                // There are less items than before. Shift to the left `count` by the difference
                // between `previous_number_of_items` and `next_number_of_items` to keep the
                // same number of items in the stream as much as possible.
                //
                // This is not a backwards pagination, it cannot “go below 0”, however this is
                // necessary to handle the case where the timeline is cleared and
                // the number of items becomes 0 for example.
                if next_number_of_items < previous_number_of_items {
                    current_count.saturating_sub(previous_number_of_items - next_number_of_items)
                }
                // Return `current_count` with no modification, we don't want to adjust the
                // count, we want to see all initial items and new items.
                else {
                    current_count
                }
            }
        }

        /// Compute the `count` value for [the `Skip` higher-order
        /// stream][`Skip`] when a backwards pagination is happening.
        ///
        /// It returns the new value for `count` in addition to
        /// `Some(number_of_items)` to fulfill the page up to `page_size`,
        /// `None` otherwise. For example, assuming a `page_size` of 15,
        /// if the `count` moves from 10 to 0, then 10 new items will
        /// appear in the stream, but 5 are missing because they aren't
        /// present in the stream: the stream has reached its beginning:
        /// `Some(5)` will be returned. This is useful
        /// for the pagination mechanism to fill the timeline with more items,
        /// either from a storage, or from the network.
        ///
        /// [`Skip`]: eyeball_im_util::vector::Skip
        pub fn compute_next_when_paginating_backwards(
            &self,
            page_size: usize,
        ) -> (usize, Option<usize>) {
            let current_count = self.count.get();

            // We skip the values from the start of the timeline; paginating backwards means
            // we have to reduce the count until reaching 0.
            //
            // | `current_count` | `page_size` | output         |
            // |-----------------|-------------|----------------|
            // | 50              | 20          | (30, None)     |
            // | 30              | 20          | (10, None)     |
            // | 10              | 20          | (0, Some(10))  |
            // | 0               | 20          | (0, Some(20))  |
            //                                    ^  ^^^^^^^^
            //                                    |  |
            //                                    |  it needs 20 items to fulfill the
            //                                    |  page size
            //                                    count becomes 0
            //
            if current_count >= page_size {
                (current_count - page_size, None)
            } else {
                (0, Some(page_size - current_count))
            }
        }

        /// Compute the `count` value for [the `Skip` higher-order
        /// stream][`Skip`] when a forwards pagination is happening.
        ///
        /// The `page_size` is present to mimic the
        /// [`compute_count_when_paginating_backwards`] function but it is
        /// actually useless for the current implementation.
        ///
        /// [`Skip`]: eyeball_im_util::vector::Skip
        #[allow(unused)] // this is not used yet because only a live timeline is using it, but as soon as
        // other kind of timelines will use it, we would need it, it's better to have
        // this in case of; everything is tested, the logic is made more robust.
        pub fn compute_next_when_paginating_forwards(&self, _page_size: usize) -> usize {
            // Nothing to do, the count remains unchanged as we skip the first values, not
            // the last values; paginating forwards will add items at the end, not at the
            // start of the timeline.
            self.count.get()
        }

        /// Get the current count value.
        pub fn get(&self) -> usize {
            self.count.get()
        }

        /// Subscribe to updates of the count value.
        pub fn subscribe(&self) -> Subscriber<usize> {
            self.count.subscribe()
        }

        /// Update the skip count if and only if the timeline has a live focus
        /// ([`TimelineFocusKind::Live`]).
        pub fn update(&self, count: usize, is_live_focus: bool) {
            if is_live_focus {
                self.count.set_if_not_eq(count);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::SkipCount;

        #[test]
        fn test_compute_count_from_underflowing_initial_states() {
            let skip_count = SkipCount::new();

            // Initial state with too few new items. None is skipped.
            let previous_number_of_items = 0;
            let next_number_of_items = previous_number_of_items + 10;
            let count = skip_count.compute_next(previous_number_of_items, next_number_of_items);
            assert_eq!(count, 0);
            skip_count.count.set(count);

            // Add 5 new items. The count stays at 0 because we don't want to skip the
            // previous items.
            let previous_number_of_items = next_number_of_items;
            let next_number_of_items = previous_number_of_items + 5;
            let count = skip_count.compute_next(previous_number_of_items, next_number_of_items);
            assert_eq!(count, 0);
            skip_count.count.set(count);

            // Add 20 new items. The count stays at 0 because we don't want to
            // skip the previous items.
            let previous_number_of_items = next_number_of_items;
            let next_number_of_items = previous_number_of_items + 20;
            let count = skip_count.compute_next(previous_number_of_items, next_number_of_items);
            assert_eq!(count, 0);
            skip_count.count.set(count);

            // Remove a certain number of items. The count stays at 0 because it was
            // previously 0, no items are skipped, nothing to adjust.
            let previous_number_of_items = next_number_of_items;
            let next_number_of_items = previous_number_of_items - 4;
            let count = skip_count.compute_next(previous_number_of_items, next_number_of_items);
            assert_eq!(count, 0);
            skip_count.count.set(count);

            // Remove all items. The count goes to 0 (regardless it was 0 before).
            let previous_number_of_items = next_number_of_items;
            let next_number_of_items = 0;
            let count = skip_count.compute_next(previous_number_of_items, next_number_of_items);
            assert_eq!(count, 0);
        }

        #[test]
        fn test_compute_count_from_overflowing_initial_states() {
            let skip_count = SkipCount::new();

            // Initial state with too much new items. Some are skipped.
            let previous_number_of_items = 0;
            let next_number_of_items = previous_number_of_items + 30;
            let count = skip_count.compute_next(previous_number_of_items, next_number_of_items);
            assert_eq!(count, 10);
            skip_count.count.set(count);

            // Add 5 new items. The count stays at 10 because we don't want to skip the
            // previous items.
            let previous_number_of_items = next_number_of_items;
            let next_number_of_items = previous_number_of_items + 5;
            let count = skip_count.compute_next(previous_number_of_items, next_number_of_items);
            assert_eq!(count, 10);
            skip_count.count.set(count);

            // Add 20 new items. The count stays at 10 because we don't want to
            // skip the previous items.
            let previous_number_of_items = next_number_of_items;
            let next_number_of_items = previous_number_of_items + 20;
            let count = skip_count.compute_next(previous_number_of_items, next_number_of_items);
            assert_eq!(count, 10);
            skip_count.count.set(count);

            // Remove a certain number of items. The count is reduced by 5 so that the same
            // number of items are presented.
            let previous_number_of_items = next_number_of_items;
            let next_number_of_items = previous_number_of_items - 4;
            let count = skip_count.compute_next(previous_number_of_items, next_number_of_items);
            assert_eq!(count, 6);
            skip_count.count.set(count);

            // Remove all items. The count goes to 0 (regardless it was 6 before).
            let previous_number_of_items = next_number_of_items;
            let next_number_of_items = 0;
            let count = skip_count.compute_next(previous_number_of_items, next_number_of_items);
            assert_eq!(count, 0);
        }

        #[test]
        fn test_compute_count_when_paginating_backwards_from_underflowing_initial_states() {
            let skip_count = SkipCount::new();

            // Initial state with too few new items. None is skipped.
            let previous_number_of_items = 0;
            let next_number_of_items = previous_number_of_items + 10;
            let count = skip_count.compute_next(previous_number_of_items, next_number_of_items);
            assert_eq!(count, 0);
            skip_count.count.set(count);

            // Add 30 new items. The count stays at 0 because we don't want to skip the
            // previous items.
            let previous_number_of_items = next_number_of_items;
            let next_number_of_items = previous_number_of_items + 30;
            let count = skip_count.compute_next(previous_number_of_items, next_number_of_items);
            assert_eq!(count, 0);
            skip_count.count.set(count);

            let page_size = 20;

            // Paginate backwards.
            let (count, needs) = skip_count.compute_next_when_paginating_backwards(page_size);
            assert_eq!(count, 0);
            assert_eq!(needs, Some(20));
        }

        #[test]
        fn test_compute_count_when_paginating_backwards_from_overflowing_initial_states() {
            let skip_count = SkipCount::new();

            // Initial state with too much new items. Some are skipped.
            let previous_number_of_items = 0;
            let next_number_of_items = previous_number_of_items + 50;
            let count = skip_count.compute_next(previous_number_of_items, next_number_of_items);
            assert_eq!(count, 30);
            skip_count.count.set(count);

            // Add 30 new items. The count stays at 30 because we don't want to
            // skip the previous items.
            let previous_number_of_items = next_number_of_items;
            let next_number_of_items = previous_number_of_items + 30;
            let count = skip_count.compute_next(previous_number_of_items, next_number_of_items);
            assert_eq!(count, 30);
            skip_count.count.set(count);

            let page_size = 20;

            // Paginate backwards. The count shifts by `page_size`, and the page is full.
            let (count, needs) = skip_count.compute_next_when_paginating_backwards(page_size);
            assert_eq!(count, 10);
            assert_eq!(needs, None);
            skip_count.count.set(count);

            // Paginate backwards. The count shifts by `page_size` but reaches 0 before the
            // page becomes full. It needs 10 more items to fulfill the page.
            let (count, needs) = skip_count.compute_next_when_paginating_backwards(page_size);
            assert_eq!(count, 0);
            assert_eq!(needs, Some(10));
        }

        #[test]
        fn test_compute_count_when_paginating_forwards_from_underflowing_initial_states() {
            let skip_count = SkipCount::new();

            // Initial state with too few new items. None is skipped.
            let previous_number_of_items = 0;
            let next_number_of_items = previous_number_of_items + 10;
            let count = skip_count.compute_next(previous_number_of_items, next_number_of_items);
            assert_eq!(count, 0);
            skip_count.count.set(count);

            // Add 30 new items. The count stays at 0 because we don't want to skip the
            // previous items.
            let previous_number_of_items = next_number_of_items;
            let next_number_of_items = previous_number_of_items + 30;
            let count = skip_count.compute_next(previous_number_of_items, next_number_of_items);
            assert_eq!(count, 0);
            skip_count.count.set(count);

            let page_size = 20;

            // Paginate forwards. The count remains unchanged.
            let count = skip_count.compute_next_when_paginating_forwards(page_size);
            assert_eq!(count, 0);
        }

        #[test]
        fn test_compute_count_when_paginating_forwards_from_overflowing_initial_states() {
            let skip_count = SkipCount::new();

            // Initial state with too much new items. Some are skipped.
            let previous_number_of_items = 0;
            let next_number_of_items = previous_number_of_items + 50;
            let count = skip_count.compute_next(previous_number_of_items, next_number_of_items);
            assert_eq!(count, 30);
            skip_count.count.set(count);

            // Add 30 new items. The count stays at 30 because we don't want to
            // skip the previous items.
            let previous_number_of_items = next_number_of_items;
            let next_number_of_items = previous_number_of_items + 30;
            let count = skip_count.compute_next(previous_number_of_items, next_number_of_items);
            assert_eq!(count, 30);
            skip_count.count.set(count);

            let page_size = 20;

            // Paginate forwards. The count remains unchanged.
            let count = skip_count.compute_next_when_paginating_forwards(page_size);
            assert_eq!(count, 30);
        }
    }
}
