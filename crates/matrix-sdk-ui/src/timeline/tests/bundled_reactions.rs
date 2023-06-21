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

use ruma::{server_name, EventId, OwnedEventId, OwnedTransactionId};

use super::{ALICE, BOB};
use crate::timeline::{event_item::BundledReactionsExt, BundledReactions, ReactionGroup};

fn create_test_bundle() -> BundledReactions {
    let mut bundle = BundledReactions::new();
    let mut reaction_group_1 = ReactionGroup::default();
    reaction_group_1.0.insert(new_reaction(), ALICE.to_owned());
    reaction_group_1.0.insert(new_reaction(), BOB.to_owned());
    bundle.insert("key1".into(), reaction_group_1);

    let mut reaction_group_2 = ReactionGroup::default();
    reaction_group_2.0.insert(new_reaction(), ALICE.to_owned());
    bundle.insert("key2".into(), reaction_group_2);
    bundle
}

#[test]
fn by_key_with_multiple_reactions() {
    let bundle = create_test_bundle();

    assert_eq!(bundle.by_key("key1").unwrap().len(), 2);
}

#[test]
fn by_key_with_no_reactions() {
    let bundle = create_test_bundle();

    assert!(bundle.by_key("unknown-key").is_none());
}

fn new_reaction() -> (Option<OwnedTransactionId>, Option<OwnedEventId>) {
    let event_id = EventId::new(server_name!("example.org"));
    (None, Some(event_id))
}
