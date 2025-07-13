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

use super::{BoxedFilterFn, Filter};

/// Create a new filter that will run multiple filters. It returns `true` if at
/// least one of the filter returns `true`.
pub fn new_filter(filters: Vec<BoxedFilterFn>) -> impl Filter + use<> {
    move |room| -> bool { filters.iter().any(|filter| filter(room)) }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use matrix_sdk::test_utils::logged_in_client_with_server;
    use matrix_sdk_test::async_test;
    use ruma::room_id;

    use super::{super::new_rooms, *};

    #[async_test]
    async fn test_one_filter_is_true() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        let filter = |_: &_| true;
        let any = new_filter(vec![Box::new(filter)]);

        assert!(any(&room));
    }

    #[async_test]
    async fn test_one_filter_is_false() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        let filter = |_: &_| false;
        let any = new_filter(vec![Box::new(filter)]);

        assert!(any(&room).not());
    }

    #[async_test]
    async fn test_two_filters_with_true_true() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        let filter1 = |_: &_| true;
        let filter2 = |_: &_| true;
        let any = new_filter(vec![Box::new(filter1), Box::new(filter2)]);

        assert!(any(&room));
    }

    #[async_test]
    async fn test_two_filters_with_true_false() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        let filter1 = |_: &_| true;
        let filter2 = |_: &_| false;
        let any = new_filter(vec![Box::new(filter1), Box::new(filter2)]);

        assert!(any(&room));
    }

    #[async_test]
    async fn test_two_filters_with_false_true() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        let filter1 = |_: &_| false;
        let filter2 = |_: &_| true;
        let any = new_filter(vec![Box::new(filter1), Box::new(filter2)]);

        assert!(any(&room));
    }

    #[async_test]
    async fn test_two_filters_with_false_false() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        let filter1 = |_: &_| false;
        let filter2 = |_: &_| false;
        let any = new_filter(vec![Box::new(filter1), Box::new(filter2)]);

        assert!(any(&room).not());
    }
}
