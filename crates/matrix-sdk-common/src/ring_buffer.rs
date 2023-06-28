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

use std::collections::{vec_deque::Iter, VecDeque};

use serde::{self, Deserialize, Serialize};

/// A simple fixed-size ring buffer implementation.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct RingBuffer<T> {
    inner: VecDeque<T>,
}

impl<T> RingBuffer<T> {
    pub fn new(size: usize) -> Self {
        Self { inner: VecDeque::with_capacity(size) }
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        self.inner.get(index)
    }

    pub fn push(&mut self, value: T) {
        if self.inner.len() == self.inner.capacity() {
            self.inner.pop_front();
        }

        self.inner.push_back(value);
    }

    pub fn pop(&mut self) -> Option<T> {
        self.inner.pop_front()
    }

    pub fn iter(&self) -> Iter<'_, T> {
        self.inner.iter()
    }

    pub fn clear(&mut self) {
        self.inner.clear();
    }
}

impl<U> Extend<U> for RingBuffer<U> {
    fn extend<T: IntoIterator<Item = U>>(&mut self, iter: T) {
        for item in iter.into_iter() {
            self.push(item);
        }
    }
}

impl<T> Default for RingBuffer<T> {
    fn default() -> Self {
        Self { inner: Default::default() }
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use super::*;

    #[test]
    pub fn test_fixed_size() {
        let mut ring_buffer = RingBuffer::new(5);

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
    pub fn test_push_and_pop_and_length() {
        let mut ring_buffer = RingBuffer::new(3);

        ring_buffer.push(1);
        assert_eq!(ring_buffer.len(), 1);

        ring_buffer.push(2);
        assert_eq!(ring_buffer.len(), 2);

        ring_buffer.push(3);
        assert_eq!(ring_buffer.len(), 3);

        ring_buffer.pop();
        assert_eq!(ring_buffer.len(), 2);
        assert_eq!(ring_buffer.get(0), Some(&2));
        assert_eq!(ring_buffer.get(1), Some(&3));
        assert_eq!(ring_buffer.get(2), None);

        ring_buffer.pop();
        assert_eq!(ring_buffer.len(), 1);
        assert_eq!(ring_buffer.get(0), Some(&3));
        assert_eq!(ring_buffer.get(1), None);
        assert_eq!(ring_buffer.get(2), None);

        ring_buffer.pop();
        assert_eq!(ring_buffer.len(), 0);
        assert_eq!(ring_buffer.get(0), None);
        assert_eq!(ring_buffer.get(1), None);
        assert_eq!(ring_buffer.get(2), None);
    }

    #[test]
    fn clear_on_empty_buffer_is_a_noop() {
        let mut ring_buffer: RingBuffer<u8> = RingBuffer::new(3);
        ring_buffer.clear();
        assert_eq!(ring_buffer.len(), 0);
    }

    #[test]
    fn clear_removes_all_items() {
        // Given a RingBuffer that has been used
        let mut ring_buffer = RingBuffer::new(3);
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
    fn roundtrip_serialization() {
        // Given a RingBuffer
        let mut ring_buffer = RingBuffer::new(3);
        ring_buffer.push("1".to_owned());
        ring_buffer.push("2".to_owned());

        // When I serialize it
        let json = serde_json::to_string(&ring_buffer).expect("serialisation failed");
        // Sanity: the JSON looks as we expect
        assert_eq!(json, r#"["1","2"]"#);

        // And deserialize it
        let new_ring_buffer: RingBuffer<String> =
            serde_json::from_str(&json).expect("deserialisation failed");

        // Then I get back the same as I started with
        assert_eq!(ring_buffer, new_ring_buffer);
    }

    #[test]
    fn extending_an_empty_ringbuffer_adds_the_items() {
        // Given a RingBuffer
        let mut ring_buffer = RingBuffer::new(5);

        // When I extend it
        ring_buffer.extend(vec!["a".to_owned(), "b".to_owned()]);

        // Then the items are added
        assert_eq!(ring_buffer.iter().map(String::as_str).collect::<Vec<_>>(), vec!["a", "b"]);
    }

    #[test]
    fn extend_adds_items_to_the_end() {
        // Given a RingBuffer with something in it
        let mut ring_buffer = RingBuffer::new(5);
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
    fn extend_does_not_overflow_max_length() {
        // Given a RingBuffer with something in it
        let mut ring_buffer = RingBuffer::new(5);
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
    fn extending_a_full_ringbuffer_preserves_max_length() {
        // Given a full RingBuffer with something in it
        let mut ring_buffer = RingBuffer::new(2);
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
}
