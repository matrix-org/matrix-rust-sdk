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

use std::cmp::Ordering;

use super::{BoxedSorterFn, Sorter};

/// Create a new sorter that will run multiple sorters. When the nth sorter
/// returns [`Ordering::Equal`], the next sorter is called. It stops as soon as
/// a sorter return [`Ordering::Greater`] or [`Ordering::Less`].
///
/// This is an implementation of a lexicographic order as defined for cartesian
/// products ([learn more](https://en.wikipedia.org/wiki/Lexicographic_order#Cartesian_products)).
pub fn new_sorter(sorters: Vec<BoxedSorterFn>) -> impl Sorter {
    move |left, right| -> Ordering {
        for sorter in &sorters {
            match sorter(left, right) {
                result @ Ordering::Greater | result @ Ordering::Less => return result,
                Ordering::Equal => continue,
            }
        }

        Ordering::Equal
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::test_utils::logged_in_client_with_server;
    use matrix_sdk_test::async_test;
    use ruma::room_id;

    use super::{super::super::filters::new_rooms, *};

    #[async_test]
    async fn test_with_zero_sorter() {
        let (client, server) = logged_in_client_with_server().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        let or = new_sorter(vec![]);

        assert_eq!(or(&room_a, &room_b), Ordering::Equal);
    }

    #[async_test]
    async fn test_with_one_sorter() {
        let (client, server) = logged_in_client_with_server().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        let sorter_1 = |_: &_, _: &_| Ordering::Less;
        let or = new_sorter(vec![Box::new(sorter_1)]);

        assert_eq!(or(&room_a, &room_b), Ordering::Less);
    }

    #[async_test]
    async fn test_with_two_sorters() {
        let (client, server) = logged_in_client_with_server().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        let sorter_1 = |_: &_, _: &_| Ordering::Equal;
        let sorter_2 = |_: &_, _: &_| Ordering::Greater;
        let or = new_sorter(vec![Box::new(sorter_1), Box::new(sorter_2)]);

        assert_eq!(or(&room_a, &room_b), Ordering::Greater);
    }

    #[async_test]
    async fn test_with_more_sorters() {
        let (client, server) = logged_in_client_with_server().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        let sorter_1 = |_: &_, _: &_| Ordering::Equal;
        let sorter_2 = |_: &_, _: &_| Ordering::Equal;
        let sorter_3 = |_: &_, _: &_| Ordering::Less;
        let sorter_4 = |_: &_, _: &_| Ordering::Greater;
        let or = new_sorter(vec![
            Box::new(sorter_1),
            Box::new(sorter_2),
            Box::new(sorter_3),
            Box::new(sorter_4),
        ]);

        assert_eq!(or(&room_a, &room_b), Ordering::Less);
    }
}
