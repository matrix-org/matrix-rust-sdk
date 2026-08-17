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

//! Tests for [`TimelineFocus::MessageTypes`]: a timeline served from the
//! event cache store's `msgtype` index.

use futures_util::StreamExt as _;
use matrix_sdk::{
    assert_let_timeout,
    test_utils::mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
};
use matrix_sdk_test::{ALICE, JoinedRoomBuilder, async_test, event_factory::EventFactory};
use matrix_sdk_ui::timeline::{RoomExt, TimelineFocus, TimelineItem};
use ruma::{EventId, event_id, owned_mxc_uri, room_id};
use stream_assert::assert_pending;

/// A compact rendering of the items, for assertions: `start`, `divider`,
/// `gap(token)` or the event ID.
fn describe(items: &eyeball_im::Vector<std::sync::Arc<TimelineItem>>) -> Vec<String> {
    items
        .iter()
        .map(|item| {
            if item.is_timeline_start() {
                "start".to_owned()
            } else if item.is_date_divider() {
                "divider".to_owned()
            } else if let Some(token) = item.as_gap() {
                format!("gap({token})")
            } else if let Some(event) = item.as_event() {
                event.event_id().map(|id| id.to_string()).unwrap_or_else(|| "local".to_owned())
            } else {
                format!("{item:?}")
            }
        })
        .collect()
}

#[async_test]
async fn test_message_types_focus_shows_indexed_media_with_gaps() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    client.event_cache().subscribe().unwrap();

    let f = EventFactory::new().room(room_id).sender(*ALICE);
    let image = |event_id: &EventId| {
        f.image(format!("{event_id}.png"), owned_mxc_uri!("mxc://example.org/img"))
            .event_id(event_id)
    };

    // Two syncs, the second one limited: the room's linked chunk is
    // `[$img1, $txt0] [gap "g1"] [$txt1, $img2]`.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(image(event_id!("$img1")))
                .add_timeline_event(f.text_msg("zero").event_id(event_id!("$txt0"))),
        )
        .await;
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("one").event_id(event_id!("$txt1")))
                .add_timeline_event(image(event_id!("$img2")))
                .set_timeline_prev_batch("g1".to_owned())
                .set_timeline_limited(),
        )
        .await;

    // Note: no `/messages` mock is mounted: nothing below may hit the
    // network.
    let timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::MessageTypes {
            msgtypes: vec!["m.image".to_owned()],
            around_event: None,
        })
        .build()
        .await
        .unwrap();

    let (mut items, mut timeline_stream) = timeline.subscribe().await;

    // Everything the store knows is exposed at once, in order, with the gap
    // between the images and the timeline start on top (the room's history
    // starts with an events chunk).
    assert_eq!(describe(&items), ["start", "divider", "$img1", "gap(g1)", "$img2"], "{items:?}");
    assert_pending!(timeline_stream);

    // Nothing to paginate: it's all there.
    assert!(timeline.paginate_backwards(10).await.unwrap());
    assert_pending!(timeline_stream);

    // Resolving the gap yields an image and a text, fully (no new token):
    // the image lands between $img1 and $img2, and the gap goes away.
    server
        .mock_room_messages()
        .match_from("g1")
        .ok(RoomMessagesResponseTemplate::default().events(vec![
            // Reverse topological order.
            image(event_id!("$img1b")),
            f.text_msg("half").event_id(event_id!("$txt0b")),
        ]))
        .mock_once()
        .mount()
        .await;

    assert!(timeline.resolve_gap("g1".to_owned(), 10).await.unwrap());

    loop {
        assert_let_timeout!(Some(timeline_updates) = timeline_stream.next());
        for update in timeline_updates {
            update.apply(&mut items);
        }
        if !items.iter().any(|item| item.is_gap()) {
            break;
        }
    }
    assert_eq!(describe(&items), ["start", "divider", "$img1", "$img1b", "$img2"], "{items:?}");
    assert_pending!(timeline_stream);

    // A new image from sync is appended (the text isn't shown).
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("two").event_id(event_id!("$txt2")))
                .add_timeline_event(image(event_id!("$img3"))),
        )
        .await;

    assert_let_timeout!(Some(timeline_updates) = timeline_stream.next());
    for update in timeline_updates {
        update.apply(&mut items);
    }
    assert_eq!(
        describe(&items),
        ["start", "divider", "$img1", "$img1b", "$img2", "$img3"],
        "{items:?}"
    );

    // A limited sync with only text leaves a gap newer than every image:
    // shown at the newest end, since media may be hiding in it.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("three").event_id(event_id!("$txt3")))
                .set_timeline_prev_batch("g2".to_owned())
                .set_timeline_limited(),
        )
        .await;

    assert_let_timeout!(Some(timeline_updates) = timeline_stream.next());
    for update in timeline_updates {
        update.apply(&mut items);
    }
    assert_eq!(
        describe(&items),
        ["start", "divider", "$img1", "$img1b", "$img2", "$img3", "gap(g2)"],
        "{items:?}"
    );
    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_message_types_focus_around_an_event_pages_both_ways() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    client.event_cache().subscribe().unwrap();

    let f = EventFactory::new().room(room_id).sender(*ALICE);

    // 60 images synced in one go.
    let mut room_builder = JoinedRoomBuilder::new(room_id);
    for i in 0..60 {
        let event_id = EventId::parse(format!("$img{i}")).unwrap();
        room_builder = room_builder.add_timeline_event(
            f.image(format!("{event_id}.png"), owned_mxc_uri!("mxc://example.org/img"))
                .event_id(&event_id),
        );
    }
    let room = server.sync_room(&client, room_builder).await;

    let timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::MessageTypes {
            msgtypes: vec!["m.image".to_owned()],
            around_event: Some(event_id!("$img30").to_owned()),
        })
        .build()
        .await
        .unwrap();

    let (mut items, mut timeline_stream) = timeline.subscribe().await;

    // Half a page either side of the event, no timeline start (there's more
    // before), the date divider on top.
    let event_ids = |items: &eyeball_im::Vector<std::sync::Arc<TimelineItem>>| {
        items
            .iter()
            .filter_map(|item| item.as_event()?.event_id().map(ToOwned::to_owned))
            .collect::<Vec<_>>()
    };
    assert!(items[0].is_date_divider());
    assert_eq!(items.len(), 51);
    assert_eq!(event_ids(&items).first().map(|id| id.as_str()), Some("$img5"));
    assert_eq!(event_ids(&items).last().map(|id| id.as_str()), Some("$img54"));

    // Forwards to the end.
    assert!(timeline.paginate_forwards(10).await.unwrap());
    assert_let_timeout!(Some(timeline_updates) = timeline_stream.next());
    for update in timeline_updates {
        update.apply(&mut items);
    }
    assert_eq!(event_ids(&items).last().map(|id| id.as_str()), Some("$img59"));

    // Backwards to the start: the timeline start shows up.
    assert!(timeline.paginate_backwards(10).await.unwrap());
    loop {
        assert_let_timeout!(Some(timeline_updates) = timeline_stream.next());
        for update in timeline_updates {
            update.apply(&mut items);
        }
        if items[0].is_timeline_start() {
            break;
        }
    }
    assert_eq!(event_ids(&items).len(), 60);
}

#[async_test]
async fn test_message_types_focus_shows_a_gap_in_a_room_with_no_cached_media() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    client.event_cache().subscribe().unwrap();

    let f = EventFactory::new().room(room_id).sender(*ALICE);

    // A limited sync with only text: the room's linked chunk is
    // `[gap "g1"] [$txt1]`, and no cached event matches the filter.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("one").event_id(event_id!("$txt1")))
                .set_timeline_prev_batch("g1".to_owned())
                .set_timeline_limited(),
        )
        .await;

    let timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::MessageTypes {
            msgtypes: vec!["m.image".to_owned()],
            around_event: None,
        })
        .build()
        .await
        .unwrap();

    let (mut items, mut timeline_stream) = timeline.subscribe().await;

    // Not an empty timeline: the gap is shown (media may hide in it), and
    // there is no timeline start, since the room's start hasn't been seen.
    assert_eq!(describe(&items), ["gap(g1)"], "{items:?}");
    assert_pending!(timeline_stream);

    // The store is exhausted, but that's not the room's start.
    assert!(timeline.paginate_backwards(10).await.unwrap());
    assert_pending!(timeline_stream);

    // Resolving the gap finds the media, and the room's start.
    server
        .mock_room_messages()
        .match_from("g1")
        .ok(RoomMessagesResponseTemplate::default().events(vec![
            f.text_msg("zero").event_id(event_id!("$txt0")),
            f.image("img.png".to_owned(), owned_mxc_uri!("mxc://example.org/img"))
                .event_id(event_id!("$img0")),
        ]))
        .mock_once()
        .mount()
        .await;

    assert!(timeline.resolve_gap("g1".to_owned(), 10).await.unwrap());

    loop {
        assert_let_timeout!(Some(timeline_updates) = timeline_stream.next());
        for update in timeline_updates {
            update.apply(&mut items);
        }
        if items.front().is_some_and(|item| item.is_timeline_start()) {
            break;
        }
    }
    assert_eq!(describe(&items), ["start", "divider", "$img0"], "{items:?}");
    assert_pending!(timeline_stream);
}
