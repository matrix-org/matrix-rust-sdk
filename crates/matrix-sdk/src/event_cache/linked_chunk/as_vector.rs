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
    ops::{ControlFlow, Not},
    sync::{Arc, RwLock},
};

use eyeball_im::VectorDiff;

use super::{
    updates::{ReaderToken, Update, UpdatesInner},
    ChunkContent, ChunkIdentifier, Iter, Position,
};

/// A type alias to represent a chunk's length. This is purely for commodity.
type ChunkLength = usize;

/// A type that transforms a `Vec<Update<Item, Gap>>` (given by
/// [`ObservableUpdates::take`](super::ObservableUpdates::take)) into a
/// `Vec<VectorDiff<Item>>` (this type). Basically, it helps to consume a
/// [`LinkedChunk<CAP, Item, Gap>`](super::LinkedChunk) as if it was an
/// [`eyeball_im::ObservableVector<Item>`].
#[derive(Debug)]
pub struct AsVector<Item, Gap> {
    /// Strong reference to [`UpdatesInner`].
    updates: Arc<RwLock<UpdatesInner<Item, Gap>>>,

    /// The token to read the updates.
    token: ReaderToken,

    /// Mapper from `Update` to `VectorDiff`.
    mapper: UpdateToVectorDiff,
}

impl<Item, Gap> AsVector<Item, Gap> {
    /// Create a new [`AsVector`].
    ///
    /// `updates` is the inner value of
    /// [`ObservableUpdates`][super::updates::ObservableUpdates].
    /// It's required to read the new [`Update`]s. `token` is the
    /// [`ReaderToken`] necessary for this type to read the [`Update`]s.
    /// `chunk_iterator` is the iterator of all [`Chunk`](super::Chunk)s, used
    /// to set up its internal state.
    pub(super) fn new<const CAP: usize>(
        updates: Arc<RwLock<UpdatesInner<Item, Gap>>>,
        token: ReaderToken,
        chunk_iterator: Iter<'_, CAP, Item, Gap>,
    ) -> Self {
        // Drain previous updates so that this type is synced with `Updates`.
        {
            let mut updates = updates.write().unwrap();
            let _ = updates.take_with_token(token);
        }

        Self { updates, token, mapper: UpdateToVectorDiff::new(chunk_iterator) }
    }

    /// Take the new updates as [`VectorDiff`].
    ///
    /// It returns an empty `Vec` if there is no new `VectorDiff` for the
    /// moment.
    pub fn take(&mut self) -> Vec<VectorDiff<Item>>
    where
        Item: Clone,
    {
        let mut updates = self.updates.write().unwrap();

        self.mapper.map(updates.take_with_token(self.token))
    }
}

/// Internal type that converts [`Update`] into [`VectorDiff`].
#[derive(Debug)]
struct UpdateToVectorDiff {
    /// Pairs of all known chunks and their respective length. This is the only
    /// required data for this algorithm.
    chunks: VecDeque<(ChunkIdentifier, ChunkLength)>,
}

impl UpdateToVectorDiff {
    /// Construct [`UpdateToVectorDiff`], based on an iterator of
    /// [`Chunk`](super::Chunk)s, used to set up its own internal state.
    ///
    /// See [`Self::map`] to learn more about the algorithm.
    fn new<const CAP: usize, Item, Gap>(chunk_iterator: Iter<'_, CAP, Item, Gap>) -> Self {
        let mut initial_chunk_lengths = VecDeque::new();

        for chunk in chunk_iterator {
            initial_chunk_lengths.push_front((
                chunk.identifier(),
                match chunk.content() {
                    ChunkContent::Gap(_) => 0,
                    ChunkContent::Items(items) => items.len(),
                },
            ))
        }

        Self { chunks: initial_chunk_lengths }
    }

    /// Map several [`Update`] into [`VectorDiff`].
    ///
    /// How does this type transform `Update` into `VectorDiff`? There is no
    /// internal buffer of kind [`eyeball_im::ObservableVector<Item>`],
    /// which could have been used to generate the `VectorDiff`s. They are
    /// computed manually.
    ///
    /// The only buffered data is pairs of [`ChunkIdentifier`] and
    /// [`ChunkLength`]. The following rules must be respected (they are defined
    /// in [`Self::new`]):
    ///
    /// * A chunk of kind [`ChunkContent::Gap`] has a length of 0,
    /// * A chunk of kind [`ChunkContent::Items`] has a length equals to its
    ///   number of items,
    /// * The pairs must be ordered exactly like the chunks in [`LinkedChunk`],
    ///   i.e. the first pair must represent the first chunk, the last pair must
    ///   represent the last chunk.
    ///
    /// The only thing this algorithm does is maintaining the pairs:
    ///
    /// * [`Update::NewItemsChunk`] and [`Update::NewGapChunk`] are inserting a
    ///   new pair with a chunk length of 0 at the appropriate index,
    /// * [`Update::RemoveChunk`] is removing a pair,
    /// * [`Update::PushItems`] is increasing the length of the appropriate pair
    ///   by the number of new items, and is potentially emitting
    ///   [`VectorDiff`],
    /// * [`Update::DetachLastItems`] is decreasing the length of the
    ///   appropriate pair by the number of items to be detached; no
    ///   [`VectorDiff`] is emitted,
    /// * [`Update::StartReattachItems`] and [`Update::EndReattachItems`] are
    ///   respectively muting or unmuting the emission of [`VectorDiff`] by
    ///   [`Update::PushItems`].
    ///
    /// The only `VectorDiff` that are emitted are [`VectorDiff::Insert`] or
    /// [`VectorDiff::Append`] because a [`LinkedChunk`] is append-only.
    ///
    /// `VectorDiff::Append` is an optimisation when numerous
    /// `VectorDiff::Insert`s have to be emitted at the last position.
    ///
    /// `VectorDiff::Insert` need an index. To compute this index, the algorithm
    /// will iterate over all pairs to accumulate each chunk length until it
    /// finds the appropriate pair (given by
    /// [`Update::PushItems::at`]). This is _the offset_. To this offset, the
    /// algorithm adds the position's index of the new items (still given by
    /// [`Update::PushItems::at`]). This is _the index_. This logic works
    /// for all cases as long as pairs are maintained according to the rules
    /// hereinabove.
    ///
    /// That's a pretty memory compact and computation efficient way to map a
    /// `Vec<Update<Item, Gap>>` into a `Vec<VectorDiff<Item>>`. The larger the
    /// `LinkedChunk` capacity is, the fewer pairs the algorithm will have
    /// to handle, e.g. for 1'000 items and a `LinkedChunk` capacity of 128,
    /// it's only 8 pairs, that is 256 bytes.
    ///
    /// [`LinkedChunk`]: super::LinkedChunk
    /// [`ChunkContent::Gap`]: super::ChunkContent::Gap
    /// [`ChunkContent::Content`]: super::ChunkContent::Content
    fn map<Item, Gap>(&mut self, updates: &[Update<Item, Gap>]) -> Vec<VectorDiff<Item>>
    where
        Item: Clone,
    {
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
        //     at: Position(ChunkIdentifier(0), 1),
        //     items: vec!['w', 'x'],
        // }
        // Update::NewItemsChunk {
        //     previous: Some(ChunkIdentifier(0)),
        //     new: ChunkIdentifier(2),
        //     next: Some(ChunkIdentifier(1)),
        // }
        // Update::PushItems {
        //     at: Position(ChunkIdentifier(2), 0),
        //     items: vec!['y', 'z'],
        // }
        // ```
        //
        // Step 3, reattaching detached items:
        //
        // ```
        // Update::StartReattachItems
        // Update::PushItems {
        //     at: Position(ChunkIdentifier(2), 2),
        //     items: vec!['b']
        // }
        // Update::NewItemsChunk {
        //     previous: Some(ChunkIdentifier(2)),
        //     new: ChunkIdentifier(3),
        //     next: Some(ChunkIdentifier(1)),
        // }
        // Update::PushItems {
        //     at: Position(ChunkIdentifier(3), 0),
        //     items: vec!['c'],
        // }
        // Update::EndReattachItems
        // ```
        //
        // To ensure an optimised behaviour of this algorithm:
        //
        // * `Update::DetachLastItems` must not emit `VectorDiff::Remove`,
        //
        // * `Update::PushItems` must not emit `VectorDiff::Insert`s or
        //   `VectorDiff::Append`s if it happens after `StartReattachItems` and before
        //   `EndReattachItems`. However, `Self::chunks` must always be updated.
        //
        // From the `VectorDiff` “point of view”, this optimisation aims at avoiding
        // removing items to push them again later.
        let mut reattaching = false;
        let mut detaching = false;

        for update in updates {
            match update {
                Update::NewItemsChunk { previous, new, next }
                | Update::NewGapChunk { previous, new, next, .. } => {
                    match (previous, next) {
                        // New chunk at the end.
                        (Some(previous), None) => {
                            debug_assert!(
                                matches!(self.chunks.back(), Some((p, _)) if p == previous),
                                "Inserting new chunk at the end: The previous chunk is invalid"
                            );

                            self.chunks.push_back((*new, 0));
                        }

                        // New chunk at the beginning.
                        (None, Some(next)) => {
                            debug_assert!(
                                matches!(self.chunks.front(), Some((n, _)) if n == next),
                                "Inserting new chunk at the end: The previous chunk is invalid"
                            );

                            self.chunks.push_front((*new, 0));
                        }

                        // New chunk is inserted between 2 chunks.
                        (Some(previous), Some(next)) => {
                            let next_chunk_index = self
                                .chunks
                                .iter()
                                .position(|(chunk_identifier, _)| chunk_identifier == next)
                                // SAFETY: Assuming `LinkedChunk` and `ObservableUpdates` are not
                                // buggy, and assuming `Self::chunks` is correctly initialized, it
                                // is not possible to insert a chunk between two chunks where one
                                // does not exist. If this predicate fails, it means `LinkedChunk`
                                // or `ObservableUpdates` contain a bug.
                                .expect("Inserting new chunk: The chunk is not found");

                            debug_assert!(
                                matches!(self.chunks.get(next_chunk_index - 1), Some((p, _)) if p == previous),
                                "Inserting new chunk: The previous chunk is invalid"
                            );

                            self.chunks.insert(next_chunk_index, (*new, 0));
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
                    let chunk_index = self
                        .chunks
                        .iter()
                        .position(|(chunk_identifier, _)| {
                            chunk_identifier == expected_chunk_identifier
                        })
                        // SAFETY: Assuming `LinkedChunk` and `ObservableUpdates` are not buggy, and
                        // assuming `Self::chunks` is correctly initialized, it is not possible to
                        // remove a chunk that does not exist. If this predicate fails, it means
                        // `LinkedChunk` or `ObservableUpdates` contain a bug.
                        .expect("Removing a chunk: The chunk is not found");

                    // It's OK to ignore the result. The `chunk_index` exists because it's been
                    // found, and we don't care about its associated value.
                    let _ = self.chunks.remove(chunk_index);
                }

                Update::PushItems { at: position, items } => {
                    let number_of_chunks = self.chunks.len();
                    let (offset, (chunk_index, chunk_length)) = self.map_to_offset(position);

                    let is_pushing_back =
                        chunk_index + 1 == number_of_chunks && position.index() >= *chunk_length;

                    // Add the number of items to the chunk in `self.chunks`.
                    *chunk_length += items.len();

                    // See `reattaching` to learn more.
                    if reattaching {
                        continue;
                    }

                    // Optimisation: we can emit a `VectorDiff::Append` in this particular case.
                    if is_pushing_back && detaching.not() {
                        diffs.push(VectorDiff::Append { values: items.into() });
                    }
                    // No optimisation: let's emit `VectorDiff::Insert`.
                    else {
                        diffs.extend(items.iter().enumerate().map(|(nth, item)| {
                            VectorDiff::Insert { index: offset + nth, value: item.clone() }
                        }));
                    }
                }

                Update::RemoveItem { at } => {
                    todo!()
                }

                Update::DetachLastItems { at: position } => {
                    let expected_chunk_identifier = position.chunk_identifier();
                    let new_length = position.index();

                    let chunk_length = self
                        .chunks
                        .iter_mut()
                        .find_map(|(chunk_identifier, chunk_length)| {
                            (*chunk_identifier == expected_chunk_identifier).then_some(chunk_length)
                        })
                        // SAFETY: Assuming `LinkedChunk` and `ObservableUpdates` are not buggy, and
                        // assuming `Self::chunks` is correctly initialized, it is not possible to
                        // detach items from a chunk that does not exist. If this predicate fails,
                        // it means `LinkedChunk` or `ObservableUpdates` contain a bug.
                        .expect("Detach last items: The chunk is not found");

                    *chunk_length = new_length;

                    // Entering the _detaching_ mode.
                    detaching = true;
                }

                Update::StartReattachItems => {
                    // Entering the _reattaching_ mode.
                    reattaching = true;
                }

                Update::EndReattachItems => {
                    // Exiting the _reattaching_ mode.
                    reattaching = false;

                    // Exiting the _detaching_ mode.
                    detaching = false;
                }
            }
        }

        diffs
    }

    fn map_to_offset(&mut self, position: &Position) -> (usize, (usize, &mut usize)) {
        let expected_chunk_identifier = position.chunk_identifier();

        let (offset, (chunk_index, chunk_length)) = {
            let control_flow = self.chunks.iter_mut().enumerate().try_fold(
                position.index(),
                |offset, (chunk_index, (chunk_identifier, chunk_length))| {
                    if chunk_identifier == &expected_chunk_identifier {
                        ControlFlow::Break((offset, (chunk_index, chunk_length)))
                    } else {
                        ControlFlow::Continue(offset + *chunk_length)
                    }
                },
            );

            match control_flow {
                // Chunk has been found, and all values have been calculated as
                // expected.
                ControlFlow::Break(values) => values,

                // Chunk has not been found.
                ControlFlow::Continue(..) => {
                    // SAFETY: Assuming `LinkedChunk` and `ObservableUpdates` are not buggy, and
                    // assuming `Self::chunks` is correctly initialized, it is not possible to work
                    // on a chunk that does not exist. If this predicate fails, it means
                    // `LinkedChunk` or `ObservableUpdates` contain a bug.
                    panic!("The chunk is not found");
                }
            }
        };

        (offset, (chunk_index, chunk_length))
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;

    use imbl::{vector, Vector};

    use super::{super::LinkedChunk, VectorDiff};

    fn apply_and_assert_eq<Item>(
        accumulator: &mut Vector<Item>,
        diffs: Vec<VectorDiff<Item>>,
        expected_diffs: &[VectorDiff<Item>],
    ) where
        Item: PartialEq + Clone + Debug,
    {
        assert_eq!(diffs, expected_diffs);

        for diff in diffs {
            match diff {
                VectorDiff::Insert { index, value } => accumulator.insert(index, value),
                VectorDiff::Append { values } => accumulator.append(values),
                diff => unimplemented!("{diff:?}"),
            }
        }
    }

    #[test]
    fn test_as_vector() {
        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();
        let mut as_vector = linked_chunk.as_vector().unwrap();

        let mut accumulator = Vector::new();

        assert!(as_vector.take().is_empty());

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
        apply_and_assert_eq(
            &mut accumulator,
            as_vector.take(),
            &[
                VectorDiff::Append { values: vector!['a', 'b', 'c'] },
                VectorDiff::Append { values: vector!['d'] },
            ],
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
        apply_and_assert_eq(
            &mut accumulator,
            as_vector.take(),
            &[
                VectorDiff::Insert { index: 1, value: 'w' },
                VectorDiff::Insert { index: 2, value: 'x' },
                VectorDiff::Insert { index: 3, value: 'y' },
                VectorDiff::Insert { index: 4, value: 'z' },
            ],
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
        apply_and_assert_eq(
            &mut accumulator,
            as_vector.take(),
            &[
                VectorDiff::Append { values: vector!['e', 'f', 'g'] },
                VectorDiff::Append { values: vector!['h'] },
            ],
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
        apply_and_assert_eq(
            &mut accumulator,
            as_vector.take(),
            &[
                VectorDiff::Insert { index: 8, value: 'i' },
                VectorDiff::Insert { index: 9, value: 'j' },
                VectorDiff::Insert { index: 10, value: 'k' },
                VectorDiff::Insert { index: 11, value: 'l' },
            ],
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
        apply_and_assert_eq(
            &mut accumulator,
            as_vector.take(),
            &[VectorDiff::Insert { index: 0, value: 'm' }],
        );

        drop(linked_chunk);
        assert!(as_vector.take().is_empty());

        // Finally, ensure the “reconstitued” vector is the one expected.
        assert_eq!(
            accumulator,
            vector![
                'm', 'a', 'w', 'x', 'y', 'z', 'b', 'c', 'd', 'i', 'j', 'k', 'l', 'e', 'f', 'g', 'h'
            ]
        );
    }

    #[test]
    fn updates_are_drained_when_constructing_as_vector() {
        let mut linked_chunk = LinkedChunk::<10, char, ()>::new_with_update_history();

        linked_chunk.push_items_back(['a']);

        let mut as_vector = linked_chunk.as_vector().unwrap();
        let diffs = as_vector.take();

        // `diffs` are empty because `AsVector` is built _after_ `LinkedChunk`
        // has been updated.
        assert!(diffs.is_empty());

        linked_chunk.push_items_back(['b']);

        let diffs = as_vector.take();

        // `diffs` is not empty because new updates are coming.
        assert_eq!(diffs.len(), 1);
    }

    #[cfg(not(target_arch = "wasm32"))]
    mod proptests {
        use proptest::prelude::*;

        use super::*;

        #[derive(Debug, Clone)]
        enum AsVectorOperation {
            PushItems { items: Vec<char> },
            PushGap,
            ReplaceLastGap { items: Vec<char> },
        }

        fn as_vector_operation_strategy() -> impl Strategy<Value = AsVectorOperation> {
            prop_oneof![
                3 => prop::collection::vec(prop::char::ranges(vec!['a'..='z', 'A'..='Z'].into()), 0..=25)
                    .prop_map(|items| AsVectorOperation::PushItems { items }),

                2 => Just(AsVectorOperation::PushGap),

                1 => prop::collection::vec(prop::char::ranges(vec!['a'..='z', 'A'..='Z'].into()), 0..=25)
                    .prop_map(|items| AsVectorOperation::ReplaceLastGap { items }),
            ]
        }

        proptest! {
            #[test]
            fn as_vector_is_correct(
                operations in prop::collection::vec(as_vector_operation_strategy(), 10..=50)
            ) {
                let mut linked_chunk = LinkedChunk::<10, char, ()>::new_with_update_history();
                let mut as_vector = linked_chunk.as_vector().unwrap();

                for operation in operations {
                    match operation {
                        AsVectorOperation::PushItems { items } => {
                            linked_chunk.push_items_back(items);
                        }

                        AsVectorOperation::PushGap => {
                            linked_chunk.push_gap_back(());
                        }

                        AsVectorOperation::ReplaceLastGap { items } => {
                            let Some(gap_identifier) = linked_chunk
                                .rchunks()
                                .find_map(|chunk| chunk.is_gap().then_some(chunk.identifier()))
                            else {
                                continue;
                            };

                            linked_chunk.replace_gap_at(items, gap_identifier).unwrap();
                        }
                    }
                }

                let mut vector_from_diffs = Vec::new();

                // Read all updates as `VectorDiff` and rebuild a `Vec<char>`.
                for diff in as_vector.take() {
                    match diff {
                        VectorDiff::Insert { index, value } => vector_from_diffs.insert(index, value),
                        VectorDiff::Append { values } => {
                            let mut values = values.iter().copied().collect();

                            vector_from_diffs.append(&mut values);
                        }
                        _ => unreachable!(),
                    }
                }

                // Iterate over all chunks and collect items as `Vec<char>`.
                let vector_from_chunks = linked_chunk.items().map(|(_, item)| *item).collect::<Vec<_>>();

                // Compare both `Vec`s.
                assert_eq!(vector_from_diffs, vector_from_chunks);
            }
        }
    }
}
