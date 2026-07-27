// Copyright 2023 The Matrix.org Foundation C.I.C.
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
    collections::{
        VecDeque,
        vec_deque::{Drain, Iter, IterMut},
    },
    num::NonZeroUsize,
    ops::RangeBounds,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The capacity assigned to buffers written by older versions, which did not
/// include the logical capacity in their serialized form.
const LEGACY_DEFAULT_CAPACITY: NonZeroUsize = NonZeroUsize::new(10).unwrap();

/// A simple fixed-size ring buffer implementation.
///
/// A size is provided on creation, and the ring buffer reserves that much
/// space, and never reallocates. The logical capacity is included in the
/// serialized representation so it survives a round trip.
#[derive(Clone, Debug, PartialEq)]
pub struct RingBuffer<T> {
    inner: VecDeque<T>,

    /// The maximum number of items to hold.
    ///
    /// Tracked separately from the `VecDeque`'s own allocation capacity.
    capacity: NonZeroUsize,
}

impl<T> RingBuffer<T> {
    fn from_parts(mut inner: VecDeque<T>, capacity: NonZeroUsize) -> Self {
        while inner.len() > capacity.get() {
            inner.pop_front();
        }

        Self { inner, capacity }
    }

    /// Create a ring buffer with the supplied capacity, reserving it so we
    /// never need to reallocate.
    pub fn new(size: NonZeroUsize) -> Self {
        Self::from_parts(VecDeque::with_capacity(size.into()), size)
    }

    /// Returns the number of items that are stored in this ring buffer.
    ///
    /// This is the dynamic size indicating how many items are held in the
    /// buffer, not the fixed capacity.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns true if the ring buffer contains no elements.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Provides a reference to the element at the given index.
    ///
    /// Element at index zero is the "front" i.e. one that will be returned if
    /// we call pop().
    pub fn get(&self, index: usize) -> Option<&T> {
        self.inner.get(index)
    }

    /// Appends an element to the back of the ring buffer, dropping elements
    /// from the front if it is full.
    pub fn push(&mut self, value: T) {
        while self.inner.len() >= self.capacity.get() {
            self.inner.pop_front();
        }

        self.inner.push_back(value);
    }

    /// Removes the first element and returns it, or None if the ring buffer is
    /// empty.
    pub fn pop(&mut self) -> Option<T> {
        self.inner.pop_front()
    }

    /// Removes and returns one specific element at `index` if it exists,
    /// otherwise it returns `None`.
    pub fn remove(&mut self, index: usize) -> Option<T> {
        self.inner.remove(index)
    }

    /// Returns an iterator that provides elements in front-to-back order, i.e.
    /// the same order you would get if you repeatedly called pop().
    pub fn iter(&self) -> Iter<'_, T> {
        self.inner.iter()
    }

    /// Returns a mutable iterator that provides elements in front-to-back
    /// order, i.e. the same order you would get if you repeatedly called
    /// pop().
    pub fn iter_mut(&mut self) -> IterMut<'_, T> {
        self.inner.iter_mut()
    }

    /// Returns an iterator that drains its items.
    pub fn drain<R>(&mut self, range: R) -> Drain<'_, T>
    where
        R: RangeBounds<usize>,
    {
        self.inner.drain(range)
    }

    /// Clears the ring buffer, removing all values. This does not affect the
    /// capacity.
    pub fn clear(&mut self) {
        self.inner.clear();
    }

    /// Returns the total number of elements the `RingBuffer` can hold.
    pub fn capacity(&self) -> usize {
        self.capacity.get()
    }

    /// Retains only the elements specified by the predicate.
    pub fn retain<F>(&mut self, predicate: F)
    where
        F: FnMut(&T) -> bool,
    {
        self.inner.retain(predicate)
    }
}

// We implement this manually because `capacity` is part of the ring buffer's
// logical state but was not included in the old transparent sequence format.
// The new representation stores both the items and their capacity.
impl<T: Serialize> Serialize for RingBuffer<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct SerializedRingBuffer<'a, T> {
            items: &'a VecDeque<T>,
            capacity: NonZeroUsize,
        }

        SerializedRingBuffer { items: &self.inner, capacity: self.capacity }.serialize(serializer)
    }
}

// Accept both the new representation and the old transparent sequence format.
// The latter is needed to read RingBuffers persisted by older SDK versions;
// deriving `Deserialize` would only handle the new object representation.
impl<'a, T: Deserialize<'a>> Deserialize<'a> for RingBuffer<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'a>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum SerializedRingBuffer<T> {
            WithCapacity { items: VecDeque<T>, capacity: NonZeroUsize },
            Legacy(VecDeque<T>),
        }

        match SerializedRingBuffer::deserialize(deserializer)? {
            SerializedRingBuffer::WithCapacity { items, capacity } => {
                Ok(Self::from_parts(items, capacity))
            }
            SerializedRingBuffer::Legacy(items) => {
                let capacity = NonZeroUsize::new(items.len().max(LEGACY_DEFAULT_CAPACITY.get()))
                    .expect("legacy capacity is non-zero");
                Ok(Self::from_parts(items, capacity))
            }
        }
    }
}

impl<U> Extend<U> for RingBuffer<U> {
    fn extend<T: IntoIterator<Item = U>>(&mut self, iter: T) {
        for item in iter.into_iter() {
            self.push(item);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, ops::Not};

    use super::RingBuffer;

    #[test]
    pub fn test_fixed_size() {
        let mut ring_buffer = RingBuffer::new(NonZeroUsize::new(5).unwrap());

        assert!(ring_buffer.is_empty());

        ring_buffer.push(1);
        ring_buffer.push(2);
        ring_buffer.push(3);

        assert!(ring_buffer.is_empty().not());

        assert_eq!(ring_buffer.get(0), Some(&1));
        assert_eq!(ring_buffer.get(1), Some(&2));
        assert_eq!(ring_buffer.get(2), Some(&3));

        ring_buffer.push(4);
        ring_buffer.push(5);

        assert_eq!(ring_buffer.get(0), Some(&1));
        assert_eq!(ring_buffer.get(1), Some(&2));
        assert_eq!(ring_buffer.get(2), Some(&3));
        assert_eq!(ring_buffer.get(3), Some(&4));
        assert_eq!(ring_buffer.get(4), Some(&5));

        ring_buffer.push(6);

        assert_eq!(ring_buffer.get(0), Some(&2));
        assert_eq!(ring_buffer.get(1), Some(&3));
        assert_eq!(ring_buffer.get(2), Some(&4));
        assert_eq!(ring_buffer.get(3), Some(&5));
        assert_eq!(ring_buffer.get(4), Some(&6));
    }

    #[test]
    pub fn test_push_and_pop_and_remove_and_length() {
        let mut ring_buffer = RingBuffer::new(NonZeroUsize::new(3).unwrap());

        ring_buffer.push(1);
        assert_eq!(ring_buffer.len(), 1);

        ring_buffer.push(2);
        assert_eq!(ring_buffer.len(), 2);

        ring_buffer.push(3);
        assert_eq!(ring_buffer.len(), 3);

        assert_eq!(ring_buffer.pop(), Some(1));
        assert_eq!(ring_buffer.len(), 2);
        assert_eq!(ring_buffer.get(0), Some(&2));
        assert_eq!(ring_buffer.get(1), Some(&3));
        assert_eq!(ring_buffer.get(2), None);

        assert_eq!(ring_buffer.pop(), Some(2));
        assert_eq!(ring_buffer.len(), 1);
        assert_eq!(ring_buffer.get(0), Some(&3));
        assert_eq!(ring_buffer.get(1), None);
        assert_eq!(ring_buffer.get(2), None);

        assert_eq!(ring_buffer.pop(), Some(3));
        assert_eq!(ring_buffer.len(), 0);
        assert_eq!(ring_buffer.get(0), None);
        assert_eq!(ring_buffer.get(1), None);
        assert_eq!(ring_buffer.get(2), None);

        assert_eq!(ring_buffer.pop(), None);

        ring_buffer.push(1);
        ring_buffer.push(2);
        ring_buffer.push(3);
        assert_eq!(ring_buffer.len(), 3);
        assert_eq!(ring_buffer.get(0), Some(&1));
        assert_eq!(ring_buffer.get(1), Some(&2));
        assert_eq!(ring_buffer.get(2), Some(&3));

        assert_eq!(ring_buffer.remove(1), Some(2));
        assert_eq!(ring_buffer.len(), 2);
        assert_eq!(ring_buffer.get(0), Some(&1));
        assert_eq!(ring_buffer.get(1), Some(&3));
        assert_eq!(ring_buffer.get(2), None);

        assert_eq!(ring_buffer.remove(0), Some(1));
        assert_eq!(ring_buffer.len(), 1);
        assert_eq!(ring_buffer.get(0), Some(&3));
        assert_eq!(ring_buffer.get(1), None);
        assert_eq!(ring_buffer.get(2), None);

        assert_eq!(ring_buffer.remove(1), None);
        assert_eq!(ring_buffer.remove(10), None);
    }

    #[test]
    fn test_iter() {
        let mut ring_buffer = RingBuffer::new(NonZeroUsize::new(5).unwrap());

        ring_buffer.push(1);
        ring_buffer.push(2);
        ring_buffer.push(3);

        let as_vec = ring_buffer.iter().copied().collect::<Vec<_>>();
        assert_eq!(as_vec, [1, 2, 3]);

        let first_entry = ring_buffer.iter_mut().next().unwrap();
        *first_entry = 42;

        let as_vec = ring_buffer.iter().copied().collect::<Vec<_>>();
        assert_eq!(as_vec, [42, 2, 3]);
    }

    #[test]
    fn test_drain() {
        let mut ring_buffer = RingBuffer::new(NonZeroUsize::new(5).unwrap());

        ring_buffer.push(1);
        ring_buffer.push(2);
        ring_buffer.push(3);
        ring_buffer.push(4);
        ring_buffer.push(5);

        let drained = ring_buffer.drain(0..=2).collect::<Vec<_>>();
        let left = ring_buffer.iter().map(ToOwned::to_owned).collect::<Vec<_>>();

        assert_eq!(drained, &[1, 2, 3]);
        assert_eq!(left, &[4, 5]);

        ring_buffer.drain(..);

        assert!(ring_buffer.is_empty());
    }

    #[test]
    fn test_clear_on_empty_buffer_is_a_noop() {
        let mut ring_buffer: RingBuffer<u8> = RingBuffer::new(NonZeroUsize::new(3).unwrap());
        ring_buffer.clear();
        assert_eq!(ring_buffer.len(), 0);
    }

    #[test]
    fn test_clear_removes_all_items() {
        // Given a RingBuffer that has been used
        let mut ring_buffer = RingBuffer::new(NonZeroUsize::new(3).unwrap());
        ring_buffer.push(4);
        ring_buffer.push(5);
        ring_buffer.push(6);
        ring_buffer.pop();
        // Sanity: there are 2 items
        assert_eq!(ring_buffer.len(), 2);

        // When I clear it
        ring_buffer.clear();

        // Then it is empty
        assert_eq!(ring_buffer.len(), 0);
        assert_eq!(ring_buffer.get(0), None);
        assert_eq!(ring_buffer.pop(), None);
    }

    #[test]
    fn test_clear_does_not_affect_capacity() {
        // Given a RingBuffer that has been used
        let mut ring_buffer = RingBuffer::new(NonZeroUsize::new(3).unwrap());
        ring_buffer.push(4);
        ring_buffer.push(5);
        ring_buffer.push(6);
        ring_buffer.pop();
        // Sanity: capacity is 3
        assert_eq!(ring_buffer.capacity(), 3);

        // When I clear it
        ring_buffer.clear();

        // Then its capacity is still 3
        assert_eq!(ring_buffer.capacity(), 3);
    }

    #[test]
    fn test_capacity_is_what_we_passed_to_new() {
        // Given a RingBuffer
        let ring_buffer = RingBuffer::<i32>::new(NonZeroUsize::new(13).unwrap());
        // When I ask for its capacity I get what I provided at the start
        assert_eq!(ring_buffer.capacity(), 13);
    }

    #[test]
    fn test_capacity_is_not_affected_by_overflowing() {
        // Given a RingBuffer that has been used
        let mut ring_buffer = RingBuffer::new(NonZeroUsize::new(3).unwrap());
        ring_buffer.push(4);
        ring_buffer.push(5);
        ring_buffer.push(6);
        ring_buffer.push(7);
        ring_buffer.pop();
        ring_buffer.push(8);
        ring_buffer.push(9);

        // When I ask for its capacity, it gives me what I gave it initially
        assert_eq!(ring_buffer.capacity(), 3);

        // And even if I extend it
        ring_buffer.extend(vec![10, 11, 12, 13, 14, 15]);

        // Then its capacity is still 3
        assert_eq!(ring_buffer.capacity(), 3);
    }

    #[test]
    fn test_roundtrip_serialization() {
        // Given a RingBuffer
        let mut ring_buffer = RingBuffer::new(NonZeroUsize::new(3).unwrap());
        ring_buffer.push("1".to_owned());
        ring_buffer.push("2".to_owned());

        // When I serialize it
        let json = serde_json::to_string(&ring_buffer).expect("serialisation failed");
        // Sanity: the JSON includes the logical capacity
        assert_eq!(json, r#"{"items":["1","2"],"capacity":3}"#);

        // And deserialize it
        let new_ring_buffer: RingBuffer<String> =
            serde_json::from_str(&json).expect("deserialisation failed");

        // Then I get back the same items and capacity I started with
        assert_eq!(ring_buffer, new_ring_buffer);
        assert_eq!(new_ring_buffer.capacity(), 3);
    }

    #[test]
    fn test_deserializes_the_legacy_sequence_format() {
        let ring_buffer: RingBuffer<i32> = serde_json::from_str("[1,2]").unwrap();

        assert_eq!(ring_buffer.iter().copied().collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(ring_buffer.capacity(), 10);
    }

    #[test]
    fn test_extending_an_empty_ringbuffer_adds_the_items() {
        // Given a RingBuffer
        let mut ring_buffer = RingBuffer::new(NonZeroUsize::new(5).unwrap());

        // When I extend it
        ring_buffer.extend(vec!["a".to_owned(), "b".to_owned()]);

        // Then the items are added
        assert_eq!(ring_buffer.iter().map(String::as_str).collect::<Vec<_>>(), vec!["a", "b"]);
    }

    #[test]
    fn test_extend_adds_items_to_the_end() {
        // Given a RingBuffer with something in it
        let mut ring_buffer = RingBuffer::new(NonZeroUsize::new(5).unwrap());
        ring_buffer.push("1".to_owned());
        ring_buffer.push("2".to_owned());

        // When I extend it
        ring_buffer.extend(vec!["3".to_owned(), "4".to_owned()]);

        // Then the items are added on the end
        assert_eq!(
            ring_buffer.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["1", "2", "3", "4"]
        );
    }

    #[test]
    fn test_extend_does_not_overflow_max_length() {
        // Given a RingBuffer with something in it
        let mut ring_buffer = RingBuffer::new(NonZeroUsize::new(5).unwrap());
        ring_buffer.push("1".to_owned());
        ring_buffer.push("2".to_owned());

        // When I extend it with too many items
        ring_buffer.extend(vec![
            "3".to_owned(),
            "4".to_owned(),
            "5".to_owned(),
            "6".to_owned(),
            "7".to_owned(),
        ]);

        // Then some of previous items are gone, keeping the length to the max
        assert_eq!(
            ring_buffer.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["3", "4", "5", "6", "7"]
        );
    }

    #[test]
    fn test_extending_a_full_ringbuffer_preserves_max_length() {
        // Given a full RingBuffer with something in it
        let mut ring_buffer = RingBuffer::new(NonZeroUsize::new(2).unwrap());
        ring_buffer.push("1".to_owned());
        ring_buffer.push("2".to_owned());

        // When I extend it with lots of items
        ring_buffer.extend(vec![
            "3".to_owned(),
            "4".to_owned(),
            "5".to_owned(),
            "6".to_owned(),
            "7".to_owned(),
        ]);

        // Then only the last N items remain
        assert_eq!(ring_buffer.iter().map(String::as_str).collect::<Vec<_>>(), vec!["6", "7"]);
    }

    #[test]
    fn test_capacity_survives_a_serialization_round_trip() {
        // Given a partially filled ring buffer with a non-default capacity,
        let mut ring_buffer = RingBuffer::new(NonZeroUsize::new(3).unwrap());
        ring_buffer.push(1);

        // When it's deserialized back,
        let json = serde_json::to_string(&ring_buffer).unwrap();
        let mut ring_buffer: RingBuffer<i32> = serde_json::from_str(&json).unwrap();

        // Then it still has its original capacity and room for two more items.
        assert_eq!(ring_buffer.capacity(), 3);
        ring_buffer.push(2);
        ring_buffer.push(3);

        assert_eq!(ring_buffer.iter().copied().collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn test_retain() {
        let mut ring_buffer = RingBuffer::new(NonZeroUsize::new(2).unwrap());
        ring_buffer.push(1);
        ring_buffer.push(2);

        ring_buffer.retain(|v| v % 2 == 0);

        assert_eq!(ring_buffer.len(), 1);
        assert_eq!(ring_buffer.get(0).copied().unwrap(), 2);
    }
}
