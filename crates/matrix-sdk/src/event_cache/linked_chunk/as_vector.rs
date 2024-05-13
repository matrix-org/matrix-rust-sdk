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

type Offset = usize;
type ChunkLength = usize;

pin_project! {
    pub struct AsVectorSubscriber<Item, Gap> {
        #[pin]
        updates_subscriber: UpdatesSubscriber<Item, Gap>,

        chunks: VecDeque<(ChunkIdentifier, ChunkLength)>,
    }
}

impl<Item, Gap> AsVectorSubscriber<Item, Gap> {
    pub(super) fn new(
        updates_subscriber: UpdatesSubscriber<Item, Gap>,
        initial_chunk_lengths: VecDeque<(ChunkIdentifier, ChunkLength)>,
    ) -> Self {
        Self { updates_subscriber, chunks: initial_chunk_lengths }
    }
}

impl<Item, Gap> Stream for AsVectorSubscriber<Item, Gap>
where
    Item: Clone + std::fmt::Debug,
    Gap: Clone + std::fmt::Debug,
{
    type Item = Vec<VectorDiff<Item>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        let Some(updates) = ready!(this.updates_subscriber.as_mut().poll_next(cx)) else {
            return Poll::Ready(None);
        };

        let mut diffs = Vec::with_capacity(updates.len());

        for update in updates {
            match update {
                Update::NewItemsChunk { previous, new, next }
                | Update::NewGapChunk { previous, new, next, .. } => {
                    match (previous, next) {
                        // New chunk at the end.
                        (Some(_previous), None) => {
                            // TODO: chec `previous` is correct
                            this.chunks.push_back((new, 0));
                        }

                        // New chunk at the beginning.
                        (None, Some(_next)) => {
                            // TODO: check `next` is correct
                            this.chunks.push_front((new, 0));
                        }

                        // New chunk is inserted between 2 chunks.
                        (Some(_previous), Some(next)) => {
                            // TODO: check `previous` is correct
                            let next_chunk_index = this
                                .chunks
                                .iter()
                                .position(|(chunk_identifier, _)| *chunk_identifier == next)
                                .expect("next chunk not found");

                            this.chunks.insert(next_chunk_index, (new, 0));
                        }

                        (None, None) => {
                            unreachable!("?");
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
                        .expect("oops 4");

                    this.chunks.remove(chunk_index).unwrap();
                }

                Update::InsertItems { at: position, items } => {
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
                    .expect("`ChunkIdentifier` must exist");

                    *chunk_length += items.len();

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

                Update::TruncateItems { chunk: expected_chunk_identifier, length: new_length } => {
                    let length = this
                        .chunks
                        .iter_mut()
                        .find_map(|(chunk_identifier, length)| {
                            (*chunk_identifier == expected_chunk_identifier).then(|| length)
                        })
                        .expect("oops 3");

                    let old_length = *length;
                    *length = new_length;

                    diffs.extend(
                        (new_length..old_length)
                            .into_iter()
                            .map(|index| VectorDiff::Remove { index }),
                    );
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

        linked_chunk.push_items_back(['a', 'b']);
        linked_chunk.push_items_back(['c', 'd', 'e']);

        assert_next_eq!(
            as_vector,
            &[
                VectorDiff::Append { values: vector!['a', 'b'] },
                VectorDiff::Append { values: vector!['c'] },
                VectorDiff::Append { values: vector!['d', 'e'] },
            ]
        );

        linked_chunk
            .insert_items_at(['f', 'g'], linked_chunk.item_position(|item| *item == 'b').unwrap())
            .unwrap();

        assert_next_eq!(
            as_vector,
            &[
                VectorDiff::Remove { index: 1 },
                VectorDiff::Remove { index: 2 },
                VectorDiff::Insert { index: 1, value: 'f' },
                VectorDiff::Insert { index: 2, value: 'g' },
                VectorDiff::Insert { index: 3, value: 'b' },
                VectorDiff::Insert { index: 4, value: 'c' },
            ]
        );

        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['h', 'i']);

        assert_next_eq!(as_vector, &[VectorDiff::Append { values: vector!['h', 'i'] }]);

        linked_chunk
            .replace_gap_at(['j'], linked_chunk.chunk_identifier(|chunk| chunk.is_gap()).unwrap())
            .unwrap();

        assert_next_eq!(as_vector, &[VectorDiff::Insert { index: 7, value: 'j' }]);

        drop(linked_chunk);
        assert_closed!(as_vector);
    }
}
