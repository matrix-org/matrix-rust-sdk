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

use std::{
    collections::VecDeque,
    ops::ControlFlow,
    pin::Pin,
    task::{ready, Context, Poll},
};

use eyeball_im::VectorDiff;
use futures_core::Stream;
use pin_project_lite::pin_project;

use super::{
    updates::{Update, UpdatesSubscriber},
    ChunkIdentifier,
};

/// A type alias to represent a chunk's length. This is purely for commodity.
type ChunkLength = usize;

pin_project! {
    /// A type used to transform a `Stream<Item = Vec<Update<Item, Gap>>>` into
    /// a `Stream<Item = Vec<VectorDiff<Item>>>`. Basically, it helps to consume
    // a [`LinkedChunk<CAP, Item, Gap>`] as if it was an [`ObservableVector<Item>`].
    pub struct AsVectorSubscriber<Item, Gap> {
        // The inner `UpdatesSubscriber`.
        #[pin]
        updates_subscriber: UpdatesSubscriber<Item, Gap>,

        // Pairs of all known chunks and their respective length. This is the only
        // required data for this algorithm.
        chunks: VecDeque<(ChunkIdentifier, ChunkLength)>,
    }
}

impl<Item, Gap> AsVectorSubscriber<Item, Gap> {
    /// Create a new [`Self`].
    ///
    /// `updates_subscriber` is simply `UpdatesSubscriber`.
    /// `initial_chunk_lengths` must be pairs of all chunk identifiers with the
    /// associated chunk length. The pairs must be in the exact same order than
    /// in [`LinkedChunk`].
    ///
    /// # Safety
    ///
    /// This method is marked as unsafe only to attract the caller's attention
    /// on the order of pairs. If the order of pairs are incorrect, the entire
    /// algorithm will not work properly and is very likely to panic.
    pub(super) unsafe fn new(
        updates_subscriber: UpdatesSubscriber<Item, Gap>,
        initial_chunk_lengths: VecDeque<(ChunkIdentifier, ChunkLength)>,
    ) -> Self {
        Self { updates_subscriber, chunks: initial_chunk_lengths }
    }
}

impl<Item, Gap> Stream for AsVectorSubscriber<Item, Gap>
where
    Item: Clone,
    Gap: Clone,
{
    type Item = Vec<VectorDiff<Item>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        let Some(updates) = ready!(this.updates_subscriber.as_mut().poll_next(cx)) else {
            return Poll::Ready(None);
        };

        let mut diffs = Vec::with_capacity(updates.len());

        // A flag specifying when updates are reattaching detached items.
        //
        // Why is it useful?
        //
        // Imagine a `LinkedChunk::<3, char, ()>` containing `['a', 'b', 'c'] ['d']`. If
        // one wants to insert [`w`, x`, 'y', 'z'] at position
        // `Position(ChunkIdentifier(0), 1)`, i.e. at the position of `b`, here is what
        // happens:
        //
        // 1. `LinkedChunk` will split off `['a', 'b', 'c']` at index 1, the chunk
        //    becomes `['a']` and `b` and `c` are _detached_, thus we have:
        //
        //     ['a'] ['d']
        //
        // 2. `LinkedChunk` will then insert `w`, `x`, `y` and `z` to get:
        //
        //     ['a', 'w', 'x'] ['y', 'z'] ['d']
        //
        // 3. `LinkedChunk` will now reattach `b` and `c` after `z`, like so:
        //
        //     ['a', 'w', 'x'] ['y', 'z', 'b'] ['c'] ['d']
        //
        // This detaching/reattaching approach makes it reliable and safe. Good. Now,
        // what updates are we going to receive for each step?
        //
        // Step 1, detaching last items:
        //
        // ```
        // Update::DetachLastItems { at: Position(ChunkIdentifier(0), 1) }
        // ```
        //
        // Step 2, inserting new items:
        //
        // ```
        // Update::PushItems {
        //     position_hint: Position(ChunkIdentifier(0), 1),
        //     items: vec!['w', 'x'],
        // }
        // Update::NewItemsChunk {
        //     previous: Some(ChunkIdentifier(0)),
        //     new: ChunkIdentifier(2),
        //     next: Some(ChunkIdentifier(1)),
        // }
        // Update::PushItems {
        //     position_hint: Position(ChunkIdentifier(2), 0),
        //     items: vec!['y', 'z'],
        // }
        // ```
        //
        // Step 3, reattaching detached items:
        //
        // ```
        // Update::ReattachItems
        // Update::PushItems {
        //     position_hint: Position(ChunkIdentifier(2), 2),
        //     items: vec!['b']
        // }
        // Update::NewItemsChunk {
        //     previous: Some(ChunkIdentifier(2)),
        //     new: ChunkIdentifier(3),
        //     next: Some(ChunkIdentifier(1)),
        // }
        // Update::PushItems {
        //     position_hint: Position(ChunkIdentifier(3), 0),
        //     items: vec!['c'],
        // }
        // Update::ReattachItemsDone
        // ```
        //
        // To ensure an optimised behaviour of this algorithm:
        //
        // * `Update::DetachLastItems` must not emit `VectorDiff::Remove`,
        //
        // * `Update::PushItems` must not emit `VectorDiff::Insert`s or
        //   `VectorDiff::Append`s if it happens after `ReattachItems` and before
        //   `ReattachItemsDone`. However, `Self::chunks` must always be updated.
        //
        // From the `VectorDiff` “point of view”, this optimisation aims at avoiding
        // removing items to push them again later.
        let mut mute_push_items = false;

        for update in updates {
            match update {
                Update::NewItemsChunk { previous, new, next }
                | Update::NewGapChunk { previous, new, next, .. } => {
                    match (previous, next) {
                        // New chunk at the end.
                        (Some(previous), None) => {
                            debug_assert!(
                                matches!(this.chunks.back(), Some((p, _)) if *p == previous),
                                "Inserting new chunk at the end: The previous chunk is invalid"
                            );

                            this.chunks.push_back((new, 0));
                        }

                        // New chunk at the beginning.
                        (None, Some(next)) => {
                            debug_assert!(
                                matches!(this.chunks.front(), Some((n, _)) if *n == next),
                                "Inserting new chunk at the end: The previous chunk is invalid"
                            );

                            this.chunks.push_front((new, 0));
                        }

                        // New chunk is inserted between 2 chunks.
                        (Some(previous), Some(next)) => {
                            let next_chunk_index = this
                                .chunks
                                .iter()
                                .position(|(chunk_identifier, _)| *chunk_identifier == next)
                                // SAFETY: Assuming `LinkedChunk` and `Updates` are not buggy, and
                                // assuming `Self::chunks` is correctly initialized, it is not
                                // possible to insert a chunk between two chunks where one does not
                                // exist. If this predicate fails, it means `LinkedChunk` or
                                // `Updates` contain a bug.
                                .expect("Inserting new chunk: The chunk is not found");

                            debug_assert!(
                                matches!(this.chunks.get(next_chunk_index - 1), Some((p, _)) if *p == previous),
                                "Inserting new chunk: The previous chunk is invalid"
                            );

                            this.chunks.insert(next_chunk_index, (new, 0));
                        }

                        (None, None) => {
                            unreachable!(
                                "Inserting new chunk with no previous nor next chunk identifiers \
                                is impossible"
                            );
                        }
                    }
                }

                Update::RemoveChunk(expected_chunk_identifier) => {
                    let chunk_index = this
                        .chunks
                        .iter()
                        .position(|(chunk_identifier, _)| {
                            *chunk_identifier == expected_chunk_identifier
                        })
                        // SAFETY: Assuming `LinkedChunk` and `Updates` are not buggy, and assuming
                        // `Self::chunks` is correctly initialized, it is not possible to remove a
                        // chunk that does not exist. If this predicate fails, it means
                        // `LinkedChunk` or `Updates` contain a bug.
                        .expect("Removing a chunk: The chunk is not found");

                    // It's OK to ignore the result. The `chunk_index` exists because it's been
                    // found, and we don't care about its associated value.
                    let _ = this.chunks.remove(chunk_index);
                }

                Update::PushItems { position_hint: position, items } => {
                    let expected_chunk_identifier = position.chunk_identifier();

                    let (chunk_index, offset, chunk_length) = {
                        let control_flow = this.chunks.iter_mut().enumerate().try_fold(
                            position.index(),
                            |offset, (chunk_index, (chunk_identifier, chunk_length))| {
                                if chunk_identifier == &expected_chunk_identifier {
                                    ControlFlow::Break((chunk_index, offset, chunk_length))
                                } else {
                                    ControlFlow::Continue(offset + *chunk_length)
                                }
                            },
                        );

                        match control_flow {
                            ControlFlow::Break(value) => Some(value),
                            ControlFlow::Continue(..) => None,
                        }
                    }
                    // SAFETY: Assuming `LinkedChunk` and `Updates` are not buggy, and assuming
                    // `Self::chunks` is correctly initialized, it is not possible to push items on
                    // a chunk that does not exist. If this predicate fails, it means `LinkedChunk`
                    // or `Updates` contain a bug.
                    .expect("Pushing items: The chunk is not found");

                    *chunk_length += items.len();

                    // See `mute_push_items` to learn more.
                    if mute_push_items {
                        continue;
                    }

                    // Optimisation: we can emit a `VectorDiff::Append` in this particular case.
                    if chunk_index == this.chunks.len() - 1 {
                        diffs.push(VectorDiff::Append { values: items.into() });
                    }
                    // No optimisation: let's emit `VectorDiff::Insert`.
                    else {
                        diffs.extend(items.into_iter().enumerate().map(|(nth, item)| {
                            VectorDiff::Insert { index: offset + nth, value: item }
                        }));
                    }
                }

                Update::DetachLastItems { at } => {
                    let expected_chunk_identifier = at.chunk_identifier();
                    let new_length = at.index();

                    let length = this
                        .chunks
                        .iter_mut()
                        .find_map(|(chunk_identifier, length)| {
                            (*chunk_identifier == expected_chunk_identifier).then(|| length)
                        })
                        // SAFETY: Assuming `LinkedChunk` and `Updates` are not buggy, and assuming
                        // `Self::chunks` is correctly initialized, it is not possible to detach
                        // items from a chunk that does not exist. If this predicate fails, it means
                        // `LinkedChunk` or `Updates` contain a bug.
                        .expect("Detach last items: The chunk is not found");

                    *length = new_length;
                }

                Update::ReattachItems => {
                    // Entering the `reattaching` mode.
                    mute_push_items = true;
                }

                Update::ReattachItemsDone => {
                    // Exiting the `reattaching` mode.
                    mute_push_items = false;
                }
            }
        }

        Poll::Ready(Some(diffs))
    }
}

#[cfg(test)]
mod tests {
    use futures_util::pin_mut;
    use imbl::vector;
    use stream_assert::{assert_closed, assert_next_eq, assert_pending};

    use super::{super::LinkedChunk, VectorDiff};

    #[test]
    fn test_as_vector() {
        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();
        let as_vector = linked_chunk.subscribe_as_vector().unwrap();
        pin_mut!(as_vector);

        assert_pending!(as_vector);

        linked_chunk.push_items_back(['a', 'b', 'c', 'd']);
        #[rustfmt::skip]
        assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['d']);

        // From an `ObservableVector` point of view, it would look like:
        //
        // 0   1   2   3   4
        // +---+---+---+---+
        // | a | b | c | d |
        // +---+---+---+---+
        // ^^^^^^^^^^^^^^^^
        // |
        // new
        assert_next_eq!(
            as_vector,
            &[
                VectorDiff::Append { values: vector!['a', 'b', 'c'] },
                VectorDiff::Append { values: vector!['d'] },
            ]
        );

        linked_chunk
            .insert_items_at(
                ['w', 'x', 'y', 'z'],
                linked_chunk.item_position(|item| *item == 'b').unwrap(),
            )
            .unwrap();
        assert_items_eq!(linked_chunk, ['a', 'w', 'x'] ['y', 'z', 'b'] ['c'] ['d']);

        // From an `ObservableVector` point of view, it would look like:
        //
        // 0   1   2   3   4   5   6   7   8
        // +---+---+---+---+---+---+---+---+
        // | a | w | x | y | z | b | c | d |
        // +---+---+---+---+---+---+---+---+
        //     ^^^^^^^^^^^^^^^^
        //     |
        //     new
        assert_next_eq!(
            as_vector,
            &[
                VectorDiff::Insert { index: 1, value: 'w' },
                VectorDiff::Insert { index: 2, value: 'x' },
                VectorDiff::Insert { index: 3, value: 'y' },
                VectorDiff::Insert { index: 4, value: 'z' },
            ]
        );

        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['e', 'f', 'g', 'h']);
        assert_items_eq!(
            linked_chunk,
            ['a', 'w', 'x'] ['y', 'z', 'b'] ['c'] ['d'] [-] ['e', 'f', 'g'] ['h']
        );

        // From an `ObservableVector` point of view, it would look like:
        //
        // 0   1   2   3   4   5   6   7   8   9   10  11  12
        // +---+---+---+---+---+---+---+---+---+---+---+---+
        // | a | w | x | y | z | b | c | d | e | f | g | h |
        // +---+---+---+---+---+---+---+---+---+---+---+---+
        //                                 ^^^^^^^^^^^^^^^^
        //                                 |
        //                                 new
        assert_next_eq!(
            as_vector,
            &[
                VectorDiff::Append { values: vector!['e', 'f', 'g'] },
                VectorDiff::Append { values: vector!['h'] }
            ]
        );

        linked_chunk
            .replace_gap_at(
                ['i', 'j', 'k', 'l'],
                linked_chunk.chunk_identifier(|chunk| chunk.is_gap()).unwrap(),
            )
            .unwrap();
        assert_items_eq!(
            linked_chunk,
            ['a', 'w', 'x'] ['y', 'z', 'b'] ['c'] ['d'] ['i', 'j', 'k'] ['l'] ['e', 'f', 'g'] ['h']
        );

        // From an `ObservableVector` point of view, it would look like:
        //
        // 0   1   2   3   4   5   6   7   8   9   10  11  12  13  14  15  16
        // +---+---+---+---+---+---+---+---+---+---+---+---+---+---+---+---+
        // | a | w | x | y | z | b | c | d | i | j | k | l | e | f | g | h |
        // +---+---+---+---+---+---+---+---+---+---+---+---+---+---+---+---+
        //                                 ^^^^^^^^^^^^^^^^
        //                                 |
        //                                 new
        assert_next_eq!(
            as_vector,
            vec![
                VectorDiff::Insert { index: 8, value: 'i' },
                VectorDiff::Insert { index: 9, value: 'j' },
                VectorDiff::Insert { index: 10, value: 'k' },
                VectorDiff::Insert { index: 11, value: 'l' },
            ]
        );

        linked_chunk
            .insert_items_at(['m'], linked_chunk.item_position(|item| *item == 'a').unwrap())
            .unwrap();
        assert_items_eq!(
            linked_chunk,
            ['m', 'a', 'w'] ['x'] ['y', 'z', 'b'] ['c'] ['d'] ['i', 'j', 'k'] ['l'] ['e', 'f', 'g'] ['h']
        );

        // From an `ObservableVector` point of view, it would look like:
        //
        // 0   1   2   3   4   5   6   7   8   9   10  11  12  13  14  15  16  17
        // +---+---+---+---+---+---+---+---+---+---+---+---+---+---+---+---+---+
        // | m | a | w | x | y | z | b | c | d | i | j | k | l | e | f | g | h |
        // +---+---+---+---+---+---+---+---+---+---+---+---+---+---+---+---+---+
        // ^^^^
        // |
        // new
        assert_next_eq!(as_vector, vec![VectorDiff::Insert { index: 0, value: 'm' }]);

        drop(linked_chunk);
        assert_closed!(as_vector);
    }
}
