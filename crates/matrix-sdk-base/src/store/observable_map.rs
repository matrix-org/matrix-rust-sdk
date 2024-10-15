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

#[cfg(not(target_arch = "wasm32"))]
mod impl_non_wasm32 {
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
        V: Clone + Send + Sync + 'static,
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
        V: Clone + Send + Sync + 'static,
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
    }
}

#[cfg(target_arch = "wasm32")]
mod impl_wasm32 {
    use std::{borrow::Borrow, collections::BTreeMap, hash::Hash};

    use eyeball_im::{Vector, VectorDiff};
    use futures_util::{stream, Stream, StreamExt};

    /// An observable map for Wasm. It's a simple wrapper around `BTreeMap`.
    #[derive(Debug)]
    pub(crate) struct ObservableMap<K, V>(BTreeMap<K, V>)
    where
        V: Clone + 'static;

    impl<K, V> ObservableMap<K, V>
    where
        K: Hash + Eq + Ord,
        V: Clone + 'static,
    {
        /// Create a new `Self`.
        pub(crate) fn new() -> Self {
            Self(BTreeMap::new())
        }

        /// Insert a new `V` in the collection.
        ///
        /// If the `V` value already exists, it will be updated to the new one.
        pub(crate) fn insert(&mut self, key: K, value: V) {
            self.0.insert(key, value);
        }

        /// Reading one `V` value based on their ID, if it exists.
        pub(crate) fn get<L>(&self, key: &L) -> Option<&V>
        where
            K: Borrow<L>,
            L: Hash + Eq + Ord + ?Sized,
        {
            self.0.get(key)
        }

        /// Reading one `V` value based on their ID, or create a new one (by
        /// using `default`).
        pub(crate) fn get_or_create<L, F>(&mut self, key: &L, default: F) -> &V
        where
            K: Borrow<L>,
            L: Hash + Eq + ?Sized + ToOwned<Owned = K>,
            F: FnOnce() -> V,
        {
            self.0.entry(key.to_owned()).or_insert_with(default)
        }

        /// Return an iterator over the existing values.
        pub(crate) fn iter(&self) -> impl Iterator<Item = &V> {
            self.0.values()
        }

        /// Get a [`Stream`] of the values.
        pub(crate) fn stream(&self) -> (Vector<V>, impl Stream<Item = Vec<VectorDiff<V>>>) {
            let values: Vector<V> = self.0.values().cloned().collect();
            let stream =
                stream::iter(vec![values.clone()]).map(|v| vec![VectorDiff::Reset { values: v }]);
            (values, stream)
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use impl_non_wasm32::ObservableMap;
#[cfg(target_arch = "wasm32")]
pub(crate) use impl_wasm32::ObservableMap;

#[cfg(test)]
mod tests {
    #[cfg(not(target_arch = "wasm32"))]
    use eyeball_im::VectorDiff;
    #[cfg(not(target_arch = "wasm32"))]
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

    #[cfg(not(target_arch = "wasm32"))]
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

        drop(map);
        assert_closed!(stream);
    }
}
