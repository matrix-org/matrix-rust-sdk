//! A generic `ChunkIterator` that operates over a `Vec`.
//!
//! This type is not designed to work over FFI, but it can be embedded inside an
//! `uniffi::Object` for example.

use std::{cmp, mem, sync::RwLock};

pub struct ChunkIterator<T> {
    items: RwLock<Vec<T>>,
}

impl<T> ChunkIterator<T> {
    pub fn new(items: Vec<T>) -> Self {
        Self { items: RwLock::new(items) }
    }

    pub fn len(&self) -> u32 {
        self.items.read().unwrap().len().try_into().unwrap()
    }

    pub fn next(&self, chunk_size: u32) -> Option<Vec<T>> {
        if self.items.read().unwrap().is_empty() {
            None
        } else if chunk_size == 0 {
            Some(Vec::new())
        } else {
            let mut items = self.items.write().unwrap();

            // Compute the `chunk_size`.
            let chunk_size = cmp::min(items.len(), chunk_size.try_into().unwrap());
            // Split the items vector.
            let mut tail = items.split_off(chunk_size);
            // `Vec::split_off` returns the tail, and `items` contains the head. Let's
            // swap them.
            mem::swap(&mut tail, &mut items);
            // Finally, let's rename `tail` to `head`.
            let head = tail;

            Some(head)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ChunkIterator;

    #[test]
    fn test_len() {
        assert_eq!(ChunkIterator::<u8>::new(vec![1, 2, 3]).len(), 3);
        assert_eq!(ChunkIterator::<u8>::new(vec![]).len(), 0);
    }

    #[test]
    fn test_next() {
        let iterator = ChunkIterator::new(vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);

        assert_eq!(iterator.next(3), Some(vec![1, 2, 3]));
        assert_eq!(iterator.next(5), Some(vec![4, 5, 6, 7, 8]));
        assert_eq!(iterator.next(0), Some(vec![]));
        assert_eq!(iterator.next(1), Some(vec![9]));
        assert_eq!(iterator.next(3), Some(vec![10, 11]));
        assert_eq!(iterator.next(2), None);
        assert_eq!(iterator.next(2), None);
    }
}
