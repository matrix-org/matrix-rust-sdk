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

/// A simple fixed-size ring buffer implementation.
#[derive(Debug)]
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
}
