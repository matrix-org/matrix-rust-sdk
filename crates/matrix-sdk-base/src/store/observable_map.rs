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

//! An [`ObservableMap`] implementation.

use std::{borrow::Borrow, collections::HashMap, hash::Hash};

use eyeball_im::{ObservableVector, Vector, VectorDiff};
use futures_util::Stream;

/// An observable map.
///
/// This is an “observable map” naive implementation. Just like regular
/// hashmap, we have a redirection from a key to a position, and from a
/// position to a value. The (key, position) tuples are stored in an
/// [`HashMap`]. The (position, value) tuples are stored in an
/// [`ObservableVector`]. The (key, position) tuple is only provided for
/// fast _reading_ implementations, like `Self::get` and
/// `Self::get_or_create`. The (position, value) tuples are observable,
/// this is what interests us the most here.
///
/// Why not implementing a new `ObservableMap` type in `eyeball-im` instead
/// of this custom implementation? Because we want to continue providing
/// `VectorDiff` when observing the changes, so that the rest of the API in
/// the Matrix Rust SDK aren't broken. Indeed, an `ObservableMap` must
/// produce `MapDiff`, which would be quite different.
/// Plus, we would like to re-use all our existing code, test, stream
/// adapters and so on.
///
/// This is a trade-off. This implementation is simple enough for the
/// moment, and basically does the job.
#[derive(Debug)]
pub(crate) struct ObservableMap<K, V>
where
    V: Clone + 'static,
{
    /// The (key, position) tuples.
    mapping: HashMap<K, usize>,

    /// The values where the indices are the `position` part of
    /// `Self::mapping`.
    values: ObservableVector<V>,
}

impl<K, V> ObservableMap<K, V>
where
    K: Hash + Eq,
    V: Clone + 'static,
{
    /// Create a new `Self`.
    pub(crate) fn new() -> Self {
        Self { mapping: HashMap::new(), values: ObservableVector::new() }
    }

    /// Insert a new `V` in the collection.
    ///
    /// If the `V` value already exists, it will be updated to the new one.
    pub(crate) fn insert(&mut self, key: K, value: V) -> usize {
        match self.mapping.get(&key) {
            Some(position) => {
                self.values.set(*position, value);

                *position
            }
            None => {
                let position = self.values.len();

                self.values.push_back(value);
                self.mapping.insert(key, position);

                position
            }
        }
    }

    /// Reading one `V` value based on their ID, if it exists.
    pub(crate) fn get<L>(&self, key: &L) -> Option<&V>
    where
        K: Borrow<L>,
        L: Hash + Eq + ?Sized,
    {
        self.mapping.get(key).and_then(|position| self.values.get(*position))
    }

    /// Reading one `V` value based on their ID, or create a new one (by
    /// using `default`).
    pub(crate) fn get_or_create<L, F>(&mut self, key: &L, default: F) -> &V
    where
        K: Borrow<L>,
        L: Hash + Eq + ?Sized + ToOwned<Owned = K>,
        F: FnOnce() -> V,
    {
        let position = match self.mapping.get(key) {
            Some(position) => *position,
            None => {
                let value = default();
                let position = self.values.len();

                self.values.push_back(value);
                self.mapping.insert(key.to_owned(), position);

                position
            }
        };

        self.values
            .get(position)
            .expect("Value should be present or has just been inserted, but it's missing")
    }

    /// Return an iterator over the existing values.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &V> {
        self.values.iter()
    }

    /// Get a [`Stream`] of the values.
    pub(crate) fn stream(&self) -> (Vector<V>, impl Stream<Item = Vec<VectorDiff<V>>>) {
        self.values.subscribe().into_values_and_batched_stream()
    }

    /// Remove a `V` value based on their ID, if it exists.
    ///
    /// Returns the removed value.
    pub(crate) fn remove<L>(&mut self, key: &L) -> Option<V>
    where
        K: Borrow<L>,
        L: Hash + Eq + ?Sized,
    {
        let position = self.mapping.remove(key)?;

        // Reindex every mapped entry that is after the position we're looking to
        // remove.
        for mapped_pos in self.mapping.values_mut().filter(|pos| **pos > position) {
            *mapped_pos = mapped_pos.saturating_sub(1);
        }

        Some(self.values.remove(position))
    }
}

#[cfg(test)]
mod tests {
    use eyeball_im::VectorDiff;
    use stream_assert::{assert_closed, assert_next_eq, assert_pending};

    use super::ObservableMap;

    #[test]
    fn test_insert_and_get() {
        let mut map = ObservableMap::<char, char>::new();

        assert!(map.get(&'a').is_none());
        assert!(map.get(&'b').is_none());
        assert!(map.get(&'c').is_none());

        // new items
        map.insert('a', 'e');
        map.insert('b', 'f');

        assert_eq!(map.get(&'a'), Some(&'e'));
        assert_eq!(map.get(&'b'), Some(&'f'));
        assert!(map.get(&'c').is_none());

        // one new item
        map.insert('c', 'g');

        assert_eq!(map.get(&'a'), Some(&'e'));
        assert_eq!(map.get(&'b'), Some(&'f'));
        assert_eq!(map.get(&'c'), Some(&'g'));

        // update one item
        map.insert('b', 'F');

        assert_eq!(map.get(&'a'), Some(&'e'));
        assert_eq!(map.get(&'b'), Some(&'F'));
        assert_eq!(map.get(&'c'), Some(&'g'));
    }

    #[test]
    fn test_get_or_create() {
        let mut map = ObservableMap::<char, char>::new();

        // insert one item
        map.insert('b', 'f');

        // get or create many items
        assert_eq!(map.get_or_create(&'a', || 'E'), &'E');
        assert_eq!(map.get_or_create(&'b', || 'F'), &'f'); // this one already exists
        assert_eq!(map.get_or_create(&'c', || 'G'), &'G');

        assert_eq!(map.get(&'a'), Some(&'E'));
        assert_eq!(map.get(&'b'), Some(&'f'));
        assert_eq!(map.get(&'c'), Some(&'G'));

        // remove non-last item
        assert_eq!(map.remove(&'b'), Some('f'));

        // get_or_create item after the removed one
        assert_eq!(map.get_or_create(&'c', || 'G'), &'G');
    }

    #[test]
    fn test_remove() {
        let mut map = ObservableMap::<char, char>::new();

        assert!(map.get(&'a').is_none());
        assert!(map.get(&'b').is_none());
        assert!(map.get(&'c').is_none());

        // new items
        map.insert('a', 'e');
        map.insert('b', 'f');
        map.insert('c', 'g');

        assert_eq!(map.get(&'a'), Some(&'e'));
        assert_eq!(map.get(&'b'), Some(&'f'));
        assert_eq!(map.get(&'c'), Some(&'g'));
        assert!(map.get(&'d').is_none());

        // remove last item
        assert_eq!(map.remove(&'c'), Some('g'));

        assert_eq!(map.get(&'a'), Some(&'e'));
        assert_eq!(map.get(&'b'), Some(&'f'));
        assert_eq!(map.get(&'c'), None);

        // remove a non-existent item
        assert_eq!(map.remove(&'c'), None);

        // remove a non-last item
        assert_eq!(map.remove(&'a'), Some('e'));
        assert_eq!(map.get(&'b'), Some(&'f'));
    }

    #[test]
    fn test_iter() {
        let mut map = ObservableMap::<char, char>::new();

        // new items
        map.insert('a', 'e');
        map.insert('b', 'f');
        map.insert('c', 'g');

        assert_eq!(
            map.iter().map(|c| c.to_ascii_uppercase()).collect::<Vec<_>>(),
            &['E', 'F', 'G']
        );
    }

    #[test]
    fn test_stream() {
        let mut map = ObservableMap::<char, char>::new();

        // insert one item
        map.insert('b', 'f');

        let (initial_values, mut stream) = map.stream();
        assert_eq!(initial_values.iter().copied().collect::<Vec<_>>(), &['f']);

        assert_pending!(stream);

        // insert two items
        map.insert('c', 'g');
        map.insert('a', 'e');
        assert_next_eq!(
            stream,
            vec![VectorDiff::PushBack { value: 'g' }, VectorDiff::PushBack { value: 'e' }]
        );

        assert_pending!(stream);

        // update one item
        map.insert('b', 'F');
        assert_next_eq!(stream, vec![VectorDiff::Set { index: 0, value: 'F' }]);

        assert_pending!(stream);

        // remove one item
        map.remove(&'b');
        assert_next_eq!(stream, vec![VectorDiff::Remove { index: 0 }]);

        assert_pending!(stream);

        drop(map);
        assert_closed!(stream);
    }
}
