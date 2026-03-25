// Copyright 2026 The Matrix.org Foundation C.I.C.
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

//! Unit tests for live location sharing (MSC3489) in the timeline.

use std::time::Duration;

use assert_matches2::assert_matches;
use eyeball_im::VectorDiff;
use matrix_sdk_test::{ALICE, BOB, async_test};
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, event_id,
    events::beacon_info::{BeaconInfoEventContent, RedactedBeaconInfoEventContent},
    owned_event_id, uint,
};
use stream_assert::{assert_next_matches, assert_pending};

use crate::timeline::{
    EventTimelineItem, ReactionStatus, TimelineEventItemId, event_item::beacon_info_matches,
    tests::TestTimeline,
};

/// A `beacon_info` state event creates a `MsgLikeKind::LiveLocation`
/// item with `is_live() == true` and no accumulated locations.
#[async_test]
async fn test_beacon_info_creates_timeline_item() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;
    let beacon_id = event_id!("$beacon_info:example.org");

    timeline
        .send_beacon_info(
            &ALICE,
            beacon_id,
            Some("Alice's walk".to_owned()),
            Duration::from_secs(3600),
            true,
            None,
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let state = item.content().as_live_location_state().expect("should be a live location item");

    assert!(state.is_live(), "beacon should be live");
    assert_eq!(state.description(), Some("Alice's walk"));
    assert!(state.locations().is_empty(), "no locations yet");

    assert_pending!(stream);
}

/// A `beacon_info` event whose timeout has expired still creates a timeline
/// item (we check `live`, not `is_live()` which would return `false`).
#[async_test]
async fn test_beacon_info_with_expired_timeout_still_creates_item() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;
    let beacon_id = event_id!("$beacon_info:example.org");

    // Use a timestamp in the past with a very short duration so that
    // is_live() would return false (expired), but `live` field is true.
    let past_ts = MilliSecondsSinceUnixEpoch(uint!(1_000)); // Very early timestamp
    let short_duration = Duration::from_millis(1); // Effectively expired immediately

    timeline.send_beacon_info(&ALICE, beacon_id, None, short_duration, true, Some(past_ts)).await;

    // The item should still be created even though the timeout has expired.
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let state = item.content().as_live_location_state().expect("should be a live location item");

    // The `live` field is true, but is_live() returns false because the
    // timeout has expired.
    assert!(!state.is_live(), "is_live() should return false because the timeout has expired");
    assert!(state.beacon_info.live, "live should be true");

    assert_pending!(stream);
}

/// A `beacon_info` event with `live: false` arriving without a prior live
/// `beacon_info` produces no timeline item (there is nothing to stop).
#[async_test]
async fn test_beacon_info_stopped_without_start_produces_no_item() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;
    let beacon_id = event_id!("$beacon_stop:example.org");

    timeline.send_beacon_info(&ALICE, beacon_id, None, Duration::from_secs(60), false, None).await;

    assert_pending!(stream);

    assert!(
        timeline.live_location_event_items().await.is_empty(),
        "a stop beacon_info without a prior start should not produce a timeline item"
    );
}

/// A `beacon` message event aggregates a location onto the parent `beacon_info`
/// timeline item.
#[async_test]
async fn test_beacon_update_aggregates_onto_beacon_info() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;
    let beacon_id = event_id!("$beacon_info:example.org");

    timeline.send_beacon_info(&ALICE, beacon_id, None, Duration::from_secs(3600), true, None).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let state = item.content().as_live_location_state().unwrap();
    assert!(state.is_live());
    assert!(state.locations().is_empty());

    let ts = MilliSecondsSinceUnixEpoch(uint!(1_000_000));
    timeline.send_beacon_location(&ALICE, beacon_id, 51.5008, 0.1247, ts).await;

    // The existing item is updated in-place via a Set diff.
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    let state = item.content().as_live_location_state().unwrap();
    assert_eq!(state.locations().len(), 1);

    let loc = state.latest_location().unwrap();
    assert!(
        loc.geo_uri().starts_with("geo:51.5008,0.1247"),
        "unexpected geo_uri: {}",
        loc.geo_uri()
    );
    assert_eq!(loc.ts(), ts);

    assert_pending!(stream);
}

/// Multiple location updates accumulate in timestamp order.
#[async_test]
async fn test_multiple_beacon_updates_accumulate_in_order() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;
    let beacon_id = event_id!("$beacon_info:example.org");

    timeline.send_beacon_info(&ALICE, beacon_id, None, Duration::from_secs(3600), true, None).await;
    let _item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

    // Send in non-chronological order.
    let ts2 = MilliSecondsSinceUnixEpoch(uint!(2_000));
    let ts1 = MilliSecondsSinceUnixEpoch(uint!(1_000));
    let ts3 = MilliSecondsSinceUnixEpoch(uint!(3_000));

    timeline.send_beacon_location(&ALICE, beacon_id, 10.0, 20.0, ts2).await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_eq!(item.content().as_live_location_state().unwrap().locations().len(), 1);

    timeline.send_beacon_location(&ALICE, beacon_id, 11.0, 21.0, ts1).await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_eq!(item.content().as_live_location_state().unwrap().locations().len(), 2);

    timeline.send_beacon_location(&ALICE, beacon_id, 12.0, 22.0, ts3).await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    let state = item.content().as_live_location_state().unwrap();
    assert_eq!(state.locations().len(), 3);

    // latest_location should be the one with the highest ts.
    assert_eq!(state.latest_location().unwrap().ts(), ts3);

    // Verify sort order: ts1 < ts2 < ts3.
    let tss: Vec<_> = state.locations().iter().map(|l| l.ts()).collect();
    assert_eq!(tss, vec![ts1, ts2, ts3]);

    assert_pending!(stream);
}

/// When a `beacon` message event arrives *before* its parent `beacon_info`
/// state event, the aggregation is stashed and applied once the parent appears.
#[async_test]
async fn test_beacon_update_before_beacon_info_is_applied_when_parent_arrives() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;
    let beacon_id: OwnedEventId = owned_event_id!("$beacon:example.org");

    // Location update arrives first – no parent yet.
    let ts = MilliSecondsSinceUnixEpoch(uint!(5_000));
    timeline.send_beacon_location(&ALICE, &beacon_id, 48.8584, 2.2945, ts).await;

    // The location update event is filtered out from the visible items because
    // beacon message events are never shown as standalone items.
    assert_pending!(stream);
    assert!(
        timeline.live_location_event_items().await.is_empty(),
        "beacon update should not appear standalone"
    );

    // Now the beacon_info arrives.
    timeline
        .send_beacon_info(&ALICE, &beacon_id, None, Duration::from_secs(3600), true, None)
        .await;

    // A single PushBack with the stashed location already applied.
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let state = item.content().as_live_location_state().unwrap();
    assert!(state.is_live());
    assert_eq!(state.locations().len(), 1);
    assert_eq!(state.latest_location().unwrap().ts(), ts);

    assert_pending!(stream);
}

/// Two independent users sharing their live location produce two separate
/// `Beacon` timeline items.
#[async_test]
async fn test_multiple_users_sharing_produce_independent_items() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;
    let alice_beacon_id = event_id!("$alice_beacon:example.org");
    let bob_beacon_id = event_id!("$bob_beacon:example.org");

    timeline
        .send_beacon_info(
            &ALICE,
            alice_beacon_id,
            Some("Alice".to_owned()),
            Duration::from_secs(3600),
            true,
            None,
        )
        .await;

    let alice_item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_eq!(alice_item.sender(), *ALICE);
    assert_eq!(alice_item.content().as_live_location_state().unwrap().description(), Some("Alice"));

    timeline
        .send_beacon_info(
            &BOB,
            bob_beacon_id,
            Some("Bob".to_owned()),
            Duration::from_secs(3600),
            true,
            None,
        )
        .await;

    let bob_item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_eq!(bob_item.sender(), *BOB);
    assert_eq!(bob_item.content().as_live_location_state().unwrap().description(), Some("Bob"));

    assert_pending!(stream);

    // Location updates for Alice don't affect Bob's item.
    let ts = MilliSecondsSinceUnixEpoch(uint!(1_000));
    timeline.send_beacon_location(&ALICE, alice_beacon_id, 1.0, 2.0, ts).await;

    // Only Alice's item (index 0) is updated.
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_eq!(item.sender(), *ALICE);
    assert_eq!(item.content().as_live_location_state().unwrap().locations().len(), 1);

    assert_pending!(stream);

    // Confirm final state via snapshot.
    let items = timeline.live_location_event_items().await;
    assert_eq!(items.len(), 2, "should have one item per user");

    let alice_state = items
        .iter()
        .find(|i| i.sender() == (*ALICE))
        .unwrap()
        .content()
        .as_live_location_state()
        .unwrap();

    let bob_state = items
        .iter()
        .find(|i| i.sender() == (*BOB))
        .unwrap()
        .content()
        .as_live_location_state()
        .unwrap();

    assert_eq!(alice_state.locations().len(), 1);
    assert!(bob_state.locations().is_empty());
}

/// A `beacon` location-update event is not shown as a standalone timeline item;
/// it is silently aggregated (or stashed).
#[async_test]
async fn test_beacon_update_not_shown_standalone() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;
    let beacon_id = event_id!("$some_beacon:example.org");

    let ts = MilliSecondsSinceUnixEpoch(uint!(1_000));
    timeline.send_beacon_location(&ALICE, beacon_id, 0.0, 0.0, ts).await;

    assert_pending!(stream);

    assert!(
        timeline.live_location_event_items().await.is_empty(),
        "beacon message events must never appear as standalone items"
    );
}

/// A stop `beacon_info` (live = false) updates the existing start item
/// in-place rather than creating a new timeline item.
#[async_test]
async fn test_beacon_stop_updates_existing_item() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;
    let start_id = event_id!("$beacon_start:example.org");
    let stop_id = event_id!("$beacon_stop:example.org");

    // Both start and stop use the same session timestamp.
    let session_ts = MilliSecondsSinceUnixEpoch::now();

    timeline
        .send_beacon_info(&ALICE, start_id, None, Duration::from_secs(3600), true, Some(session_ts))
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert!(item.content().as_live_location_state().unwrap().is_live());
    assert_eq!(item.event_id().unwrap(), start_id);

    assert_pending!(stream);

    timeline
        .send_beacon_info(&ALICE, stop_id, None, Duration::from_secs(3600), false, Some(session_ts))
        .await;

    // The existing item is updated — a Set diff, not a PushBack.
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    // The original start item is updated — its event_id remains the start event.
    assert_eq!(item.event_id().unwrap(), start_id);
    assert!(
        !item.content().as_live_location_state().unwrap().is_live(),
        "the item should now report not live after the stop event"
    );

    assert_pending!(stream);
}

/// A stop `beacon_info` preserves the accumulated location updates on the
/// existing item.
#[async_test]
async fn test_beacon_stop_preserves_locations() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;
    let start_id = event_id!("$beacon_start:example.org");
    let stop_id = event_id!("$beacon_stop:example.org");

    // Both start and stop use the same session timestamp.
    let session_ts = MilliSecondsSinceUnixEpoch::now();

    timeline
        .send_beacon_info(&ALICE, start_id, None, Duration::from_secs(3600), true, Some(session_ts))
        .await;
    let _item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

    let location_ts = MilliSecondsSinceUnixEpoch(uint!(1_000));
    timeline.send_beacon_location(&ALICE, start_id, 51.5, 0.1, location_ts).await;

    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_eq!(item.content().as_live_location_state().unwrap().locations().len(), 1);

    // Stop the session.
    timeline
        .send_beacon_info(&ALICE, stop_id, None, Duration::from_secs(3600), false, Some(session_ts))
        .await;

    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    let state = item.content().as_live_location_state().unwrap();
    assert!(!state.is_live(), "should be stopped");
    assert_eq!(state.locations().len(), 1, "location updates should be preserved after stop");
    assert_eq!(state.latest_location().unwrap().ts(), location_ts);

    assert_pending!(stream);
}

/// A stop `beacon_info` that arrives **before** the live start item is stashed
/// and applied as soon as the start item is inserted. The resulting timeline
/// item should be non-live from the moment it first appears.
#[async_test]
async fn test_beacon_stop_before_start_is_applied_later() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;
    let start_id = event_id!("$beacon_start:example.org");
    let stop_id = event_id!("$beacon_stop:example.org");

    // Both start and stop use the same session timestamp.
    let session_ts = MilliSecondsSinceUnixEpoch::now();

    // Send the stop event first — the live start item doesn't exist yet.
    timeline
        .send_beacon_info(&ALICE, stop_id, None, Duration::from_secs(3600), false, Some(session_ts))
        .await;

    // No item should have been added to the timeline yet.
    assert_pending!(stream);
    assert!(
        timeline.live_location_event_items().await.is_empty(),
        "a stop-only beacon_info must not produce a standalone timeline item"
    );

    // Now send the live start event.
    timeline
        .send_beacon_info(&ALICE, start_id, None, Duration::from_secs(3600), true, Some(session_ts))
        .await;

    // The item should appear already stopped — a single PushBack, no follow-up Set.
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_eq!(item.event_id().unwrap(), start_id, "the item should carry the start event's ID");
    assert!(
        !item.content().as_live_location_state().unwrap().is_live(),
        "the item should already be non-live because the stop was received first"
    );

    assert_pending!(stream);
}

/// A pending beacon stop from an OLD session should NOT be applied to a NEW
/// session. This tests the scenario where:
/// 1. User starts session A
/// 2. Stop for session A arrives out-of-order and is stashed
/// 3. User starts session B — the stashed stop should NOT apply
#[async_test]
async fn test_pending_beacon_stop_not_applied_to_different_session() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;
    let old_stop_id = event_id!("$old_stop:example.org");
    let new_start_id = event_id!("$new_start:example.org");

    // Use the current time as base for session B.
    // Session A's timestamp doesn't matter since its stop will be discarded.
    // Session B needs a recent timestamp so is_live() doesn't fail due to timeout.
    let old_session_ts = MilliSecondsSinceUnixEpoch(uint!(1)); // Old session (past)
    let new_session_ts = MilliSecondsSinceUnixEpoch::now(); // New session (now)

    // A stop event from the OLD session arrives first (out-of-order).
    // Its corresponding start event is missing (maybe it's from long ago).
    timeline
        .send_beacon_info(
            &ALICE,
            old_stop_id,
            None,
            Duration::from_secs(3600),
            false,
            Some(old_session_ts),
        )
        .await;

    // No item should appear — there's no start item for this stop.
    assert_pending!(stream);

    // Now a NEW session starts with a DIFFERENT timestamp.
    timeline
        .send_beacon_info(
            &ALICE,
            new_start_id,
            Some("New session".to_owned()),
            Duration::from_secs(3600),
            true,
            Some(new_session_ts),
        )
        .await;

    // The new session should appear and remain LIVE because the pending stop
    // belongs to a different session (different ts).
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let state = item.content().as_live_location_state().expect("should be a live location item");

    assert!(
        state.is_live(),
        "the new session should be live; the old session's stop should NOT apply"
    );
    assert_eq!(state.description(), Some("New session"));

    assert_pending!(stream);
}

/// Duplicate beacon location updates (same timestamp) are de-duplicated.
#[async_test]
async fn test_duplicate_beacon_location_is_deduplicated() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;
    let beacon_id = event_id!("$beacon_info:example.org");

    timeline.send_beacon_info(&ALICE, beacon_id, None, Duration::from_secs(3600), true, None).await;
    let _item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

    let ts = MilliSecondsSinceUnixEpoch(uint!(1_000));
    timeline.send_beacon_location(&ALICE, beacon_id, 1.0, 2.0, ts).await;

    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_eq!(item.content().as_live_location_state().unwrap().locations().len(), 1);

    // A second event with the same ts (e.g. from a retried decryption pass).
    timeline.send_beacon_location(&ALICE, beacon_id, 1.0, 2.0, ts).await;

    // The aggregation system still emits a Set diff, but the duplicate
    // location is silently dropped so the count stays at 1.
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_eq!(
        item.content().as_live_location_state().unwrap().locations().len(),
        1,
        "duplicate timestamp should be de-duplicated"
    );

    assert_pending!(stream);
}

/// A redacted `beacon_info` event produces a redacted item (not a Beacon item).
#[async_test]
async fn test_redacted_beacon_info_produces_redacted_item() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    timeline
        .handle_live_event(timeline.factory.redacted_state(
            &ALICE,
            ALICE.as_str(),
            RedactedBeaconInfoEventContent::new(),
        ))
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert!(
        item.content().is_redacted(),
        "a redacted beacon_info should produce a redacted timeline item"
    );

    assert_pending!(stream);
}

/// A reaction on a live location item is aggregated onto the item and exposed
/// via `reactions()`.
#[async_test]
async fn test_reaction_on_live_location_item() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;
    let beacon_id = event_id!("$beacon_info:example.org");

    timeline.send_beacon_info(&ALICE, beacon_id, None, Duration::from_secs(3600), true, None).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert!(item.content().as_live_location_state().is_some());
    assert!(item.content().reactions().unwrap().is_empty());

    // BOB reacts to the live location item.
    timeline.handle_live_event(timeline.factory.reaction(beacon_id, "👍").sender(&BOB)).await;

    // The item is updated in-place with the reaction applied.
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert!(item.content().as_live_location_state().is_some(), "still a live location item");

    let reactions = item.content().reactions().expect("live location should expose reactions");
    let thumbs_up = reactions.get("👍").expect("👍 reaction should be present");
    let reaction = thumbs_up.get(*BOB).expect("BOB's reaction should be present");
    assert_matches!(&reaction.status, ReactionStatus::RemoteToRemote(_));

    assert_pending!(stream);
}

/// Multiple reactions from different senders are all aggregated onto a live
/// location item.
#[async_test]
async fn test_multiple_reactions_on_live_location_item() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;
    let beacon_id = event_id!("$beacon_info:example.org");

    timeline.send_beacon_info(&ALICE, beacon_id, None, Duration::from_secs(3600), true, None).await;
    let _item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

    // ALICE and BOB both react, with different keys.
    timeline.handle_live_event(timeline.factory.reaction(beacon_id, "👍").sender(&ALICE)).await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    let reactions = item.content().reactions().unwrap();
    assert_eq!(reactions.len(), 1);
    assert!(reactions.get("👍").unwrap().get(*ALICE).is_some());

    timeline.handle_live_event(timeline.factory.reaction(beacon_id, "❤️").sender(&BOB)).await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);

    let reactions = item.content().reactions().unwrap();
    assert_eq!(reactions.len(), 2, "two distinct reaction keys");
    assert!(reactions.get("👍").unwrap().get(*ALICE).is_some());
    assert!(reactions.get("❤️").unwrap().get(*BOB).is_some());

    assert_pending!(stream);
}

/// A reaction that arrives *before* its target live location item is stashed
/// and applied once the beacon_info item is inserted.
#[async_test]
async fn test_reaction_before_live_location_item_is_applied_when_parent_arrives() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;
    let beacon_id = event_id!("$beacon_info:example.org");

    // Reaction arrives before the beacon_info.
    timeline.handle_live_event(timeline.factory.reaction(beacon_id, "👍").sender(&BOB)).await;

    // Nothing visible yet — no parent item to attach to.
    assert_pending!(stream);

    // Now the beacon_info arrives.
    timeline.send_beacon_info(&ALICE, beacon_id, None, Duration::from_secs(3600), true, None).await;

    // The item is inserted with the reaction already applied.
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert!(item.content().as_live_location_state().is_some());

    let reactions = item.content().reactions().expect("live location should expose reactions");
    let thumbs_up = reactions.get("👍").expect("👍 reaction should be present");
    assert!(thumbs_up.get(*BOB).is_some(), "BOB's reaction should be pre-applied");

    assert_pending!(stream);
}

/// A locally-toggled reaction on a live location item produces a local echo
/// and is then confirmed by the remote echo from sync.
#[async_test]
async fn test_local_reaction_on_live_location_item() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;
    let beacon_id = event_id!("$beacon_info:example.org");

    timeline.send_beacon_info(&ALICE, beacon_id, None, Duration::from_secs(3600), true, None).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let item_id = TimelineEventItemId::EventId(item.event_id().unwrap().to_owned());

    // Toggle a reaction locally.
    timeline.toggle_reaction_local(&item_id, "👍").await.unwrap();

    // The item is updated with a local-echo reaction.
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert!(item.content().as_live_location_state().is_some());
    let reactions = item.content().reactions().unwrap();
    let reaction = reactions.get("👍").unwrap().get(*ALICE).unwrap();
    assert_matches!(
        &reaction.status,
        (ReactionStatus::LocalToLocal(_) | ReactionStatus::LocalToRemote(_))
    );

    // Receive the remote echo from sync.
    timeline.handle_live_event(timeline.factory.reaction(beacon_id, "👍").sender(&ALICE)).await;

    // The item is updated once more — now the reaction is a confirmed remote echo.
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert!(item.content().as_live_location_state().is_some());
    let reactions = item.content().reactions().unwrap();
    let reaction = reactions.get("👍").unwrap().get(*ALICE).unwrap();
    assert_matches!(&reaction.status, ReactionStatus::RemoteToRemote(_));

    assert_pending!(stream);
}

/// `beacon_info_matches` returns true for identical content.
#[test]
fn test_beacon_info_matches_identical_content() {
    let ts = MilliSecondsSinceUnixEpoch(uint!(1_000_000));
    let a = BeaconInfoEventContent::new(
        Some("Test".to_owned()),
        Duration::from_secs(3600),
        true,
        Some(ts),
    );
    let b = BeaconInfoEventContent::new(
        Some("Test".to_owned()),
        Duration::from_secs(3600),
        true,
        Some(ts),
    );

    assert!(beacon_info_matches(&a, &b));
}

/// `beacon_info_matches` ignores the `live` field (start vs stop).
#[test]
fn test_beacon_info_matches_ignores_live_field() {
    let ts = MilliSecondsSinceUnixEpoch(uint!(1_000_000));
    let start = BeaconInfoEventContent::new(
        Some("Test".to_owned()),
        Duration::from_secs(3600),
        true,
        Some(ts),
    );
    let stop = BeaconInfoEventContent::new(
        Some("Test".to_owned()),
        Duration::from_secs(3600),
        false,
        Some(ts),
    );

    assert!(beacon_info_matches(&start, &stop), "should match even though live differs");
}

/// `beacon_info_matches` returns false for different timestamps.
#[test]
fn test_beacon_info_matches_different_ts() {
    let a = BeaconInfoEventContent::new(
        None,
        Duration::from_secs(3600),
        true,
        Some(MilliSecondsSinceUnixEpoch(uint!(1_000))),
    );
    let b = BeaconInfoEventContent::new(
        None,
        Duration::from_secs(3600),
        true,
        Some(MilliSecondsSinceUnixEpoch(uint!(2_000))),
    );

    assert!(!beacon_info_matches(&a, &b), "different ts should not match");
}

/// `beacon_info_matches` returns false for different timeouts.
#[test]
fn test_beacon_info_matches_different_timeout() {
    let ts = MilliSecondsSinceUnixEpoch(uint!(1_000_000));
    let a = BeaconInfoEventContent::new(None, Duration::from_secs(3600), true, Some(ts));
    let b = BeaconInfoEventContent::new(None, Duration::from_secs(7200), true, Some(ts));

    assert!(!beacon_info_matches(&a, &b), "different timeout should not match");
}

/// `beacon_info_matches` returns false for different descriptions.
#[test]
fn test_beacon_info_matches_different_description() {
    let ts = MilliSecondsSinceUnixEpoch(uint!(1_000_000));
    let a = BeaconInfoEventContent::new(
        Some("Session A".to_owned()),
        Duration::from_secs(3600),
        true,
        Some(ts),
    );
    let b = BeaconInfoEventContent::new(
        Some("Session B".to_owned()),
        Duration::from_secs(3600),
        true,
        Some(ts),
    );

    assert!(!beacon_info_matches(&a, &b), "different description should not match");
}

impl TestTimeline {
    /// Collect every event timeline item (no virtual items).
    async fn live_location_event_items(&self) -> Vec<EventTimelineItem> {
        self.controller.items().await.iter().filter_map(|i| i.as_event().cloned()).collect()
    }

    /// Convenience: send a `beacon_info` state event from `sender`.
    async fn send_beacon_info(
        &self,
        sender: &ruma::UserId,
        event_id: &EventId,
        description: Option<String>,
        duration: Duration,
        live: bool,
        ts: Option<MilliSecondsSinceUnixEpoch>,
    ) {
        let event = self
            .factory
            .beacon_info(description, duration, live, ts)
            .sender(sender)
            .state_key(sender)
            .event_id(event_id);
        self.handle_live_event(event).await;
    }

    /// Convenience: send a `beacon` location-update event from `sender`.
    async fn send_beacon_location(
        &self,
        sender: &ruma::UserId,
        beacon_info_event_id: &EventId,
        latitude: f64,
        longitude: f64,
        ts: MilliSecondsSinceUnixEpoch,
    ) {
        let event = self
            .factory
            .beacon(beacon_info_event_id.to_owned(), latitude, longitude, 10, Some(ts))
            .sender(sender);
        self.handle_live_event(event).await;
    }
}
