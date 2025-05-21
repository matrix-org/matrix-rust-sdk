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

use super::{Room, Sorter};

struct NameMatcher<F>
where
    F: Fn(&Room, &Room) -> (Option<String>, Option<String>),
{
    names: F,
}

impl<F> NameMatcher<F>
where
    F: Fn(&Room, &Room) -> (Option<String>, Option<String>),
{
    fn matches(&self, left: &Room, right: &Room) -> Ordering {
        let (left_name, right_name) = (self.names)(left, right);

        left_name.cmp(&right_name)
    }
}

/// Create a new sorter that will sort two [`Room`] by name, i.e. by
/// comparing their display names. A lexicographically ordering is applied, i.e.
/// "a" < "b".
pub fn new_sorter() -> impl Sorter {
    let matcher = NameMatcher {
        names: move |left, right| (left.cached_display_name(), right.cached_display_name()),
    };

    move |left, right| -> Ordering { matcher.matches(left, right) }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::test_utils::logged_in_client_with_server;
    use matrix_sdk_test::async_test;
    use ruma::room_id;

    use super::{super::super::filters::new_rooms, *};

    #[async_test]
    async fn test_with_two_names() {
        let (client, server) = logged_in_client_with_server().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `room_a` has a “greater name” than `room_b`.
        {
            let matcher = NameMatcher {
                names: |_left, _right| (Some("Foo".to_owned()), Some("Baz".to_owned())),
            };

            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Greater);
        }

        // `room_a` has a “lesser name” than `room_b`.
        {
            let matcher = NameMatcher {
                names: |_left, _right| (Some("Bar".to_owned()), Some("Baz".to_owned())),
            };

            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Less);
        }

        // `room_a` has the same name than `room_b`.
        {
            let matcher = NameMatcher {
                names: |_left, _right| (Some("Baz".to_owned()), Some("Baz".to_owned())),
            };

            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Equal);
        }
    }

    #[async_test]
    async fn test_with_one_name() {
        let (client, server) = logged_in_client_with_server().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `room_a` has a name, `room_b` has no name.
        {
            let matcher = NameMatcher { names: |_left, _right| (Some("Foo".to_owned()), None) };

            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Greater);
        }

        // `room_a` has no name, `room_b` has a name.
        {
            let matcher = NameMatcher { names: |_left, _right| (None, Some("Bar".to_owned())) };

            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Less);
        }
    }

    #[async_test]
    async fn test_with_zero_name() {
        let (client, server) = logged_in_client_with_server().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `room_a` and `room_b` has no name.
        {
            let matcher = NameMatcher { names: |_left, _right| (None, None) };

            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Equal);
        }
    }
}
