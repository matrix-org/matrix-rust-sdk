use matrix_sdk_test::{async_test, ALICE, BOB};
use ruma::{
    events::{
        beacon::BeaconEventContent, beacon_info::BeaconInfoEventContent, location::LocationContent,
        AnyMessageLikeEventContent,
    },
    server_name, EventId, OwnedEventId, UserId,
};

use crate::timeline::{
    beacons::BeaconState,
    tests::{beacons::fakes::create_beacon_info, TestTimeline},
    EventTimelineItem, TimelineItemContent,
};

#[async_test]
async fn beacon_info_is_correctly_processed_in_timeline() {
    let timeline = TestTimeline::new();
    let alice_beacon = create_beacon_info("Alice's Live location", 2300);

    timeline.send_beacon_info(&ALICE, alice_beacon).await;
    let beacon_state = timeline.beacon_state().await;

    assert_beacon_info(
        &beacon_state.beacon_info_event_content,
        &create_beacon_info("Alice's Live location", 2300),
    );
    assert!(beacon_state.last_location.is_none());
}

#[async_test]
async fn beacon_updates_location() {
    let geo_uri = "geo:51.5008,0.1247;u=35".to_string();

    let timeline = TestTimeline::new();
    timeline.send_beacon_info(&ALICE, create_beacon_info("Alice's Live location", 2300)).await;
    let beacon_info_id = timeline.beacon_info_event().await.event_id().unwrap().to_owned();

    // Alice sends her location beacon
    timeline.send_beacon(&ALICE, &beacon_info_id, geo_uri).await;
    let beacon_state = timeline.beacon_state().await;

    assert!(beacon_state.last_location.is_some());
    assert!(beacon_state.beacon_info_event_content.is_live());
}

#[async_test]
async fn beacon_updates_location_with_multiple_beacons() {
    let geo_uri = "geo:51.5008,0.1247;u=35";
    let geo_uri2 = "geo:51.5009,0.1248;u=36";

    let timeline = TestTimeline::new();
    timeline.send_beacon_info(&ALICE, create_beacon_info("Alice's Live location", 2300)).await;
    let beacon_info_id = timeline.beacon_info_event().await.event_id().unwrap().to_owned();

    // Alice sends her location beacon
    timeline.send_beacon(&ALICE, &beacon_info_id, geo_uri.to_string()).await;
    let beacon_state = timeline.beacon_state().await;

    assert!(beacon_state.last_location.is_some());
    assert_eq!(beacon_state.last_location.unwrap().uri, geo_uri);
    assert_beacon_info(
        &beacon_state.beacon_info_event_content,
        &create_beacon_info("Alice's Live location", 2300),
    );
    assert!(beacon_state.beacon_info_event_content.is_live());

    timeline.send_beacon(&ALICE, &beacon_info_id, geo_uri2.to_string()).await;
    let beacon_state = timeline.beacon_state().await;

    assert!(beacon_state.last_location.is_some());
    assert_eq!(beacon_state.last_location.unwrap().uri, geo_uri2);
    assert_beacon_info(
        &beacon_state.beacon_info_event_content,
        &create_beacon_info("Alice's Live location", 2300),
    );
    assert!(beacon_state.beacon_info_event_content.is_live());
}

#[async_test]
async fn multiple_people_sharing_location() {
    let geo_uri = "geo:51.5008,0.1247;u=35";
    let geo_uri2 = "geo:51.5009,0.1248;u=36";

    let timeline = TestTimeline::new();

    //Alice starts sharing her location
    timeline.send_beacon_info(&ALICE, create_beacon_info("Alice's Live location", 2300)).await;

    //Bob starts sharing his location
    timeline.send_beacon_info(&BOB, create_beacon_info("Bob's Live location", 2300)).await;

    let alice_beacon_info_event_id =
        timeline.event_items().await[0].clone().event_id().unwrap().to_owned();

    let bob_beacon_info_event_id =
        timeline.event_items().await[1].clone().event_id().unwrap().to_owned();

    // Alice sends her location beacon
    timeline.send_beacon(&ALICE, &alice_beacon_info_event_id, geo_uri.to_string()).await;
    let alice_beacon_state = timeline.event_items().await[0].clone().beacon_state();
    assert!(alice_beacon_state.last_location.is_some());
    assert_beacon_info(
        &alice_beacon_state.beacon_info_event_content,
        &create_beacon_info("Alice's Live location", 2300),
    );
    assert_beacon(
        alice_beacon_state.last_location.as_ref().unwrap(),
        &LocationContent::new(geo_uri.to_string()),
    );
    assert!(alice_beacon_state.beacon_info_event_content.is_live());

    //Bobs sends his location beacon
    timeline.send_beacon(&BOB, &bob_beacon_info_event_id, geo_uri2.to_string()).await;
    let bobs_beacon_state = timeline.event_items().await[1].clone().beacon_state();
    assert!(bobs_beacon_state.last_location.is_some());
    assert_beacon_info(
        &bobs_beacon_state.beacon_info_event_content,
        &create_beacon_info("Bob's Live location", 2300),
    );
    assert_beacon(
        bobs_beacon_state.last_location.as_ref().unwrap(),
        &LocationContent::new(geo_uri2.to_string()),
    );
    assert!(bobs_beacon_state.beacon_info_event_content.is_live());
}

#[async_test]
async fn beacon_info_is_stopped_by_user() {
    let timeline = TestTimeline::new();

    timeline.send_beacon_info(&ALICE, create_beacon_info("Alice's Live location", 2300)).await;
    let beacon_info_id = timeline.beacon_info_event().await.event_id().unwrap().to_owned();

    // Alice sends her location beacon
    timeline.send_beacon(&ALICE, &beacon_info_id, "geo:51.5008,0.1247;u=35".to_string()).await;
    let mut beacon_state = timeline.beacon_state().await;

    // Alice stops sharing her location
    beacon_state.beacon_info_event_content.stop();

    assert!(!beacon_state.beacon_info_event_content.is_live());
}

#[async_test]
async fn beacon_info_is_stopped_by_timeout() {
    let timeline = TestTimeline::new();

    timeline.send_beacon_info(&ALICE, create_beacon_info("Alice's Live location", 0)).await;
    let beacon_info_id = timeline.beacon_info_event().await.event_id().unwrap().to_owned();

    // Alice sends her location beacon
    timeline.send_beacon(&ALICE, &beacon_info_id, "geo:51.5008,0.1247;u=35".to_string()).await;
    let beacon_state = timeline.beacon_state().await;

    assert!(!beacon_state.beacon_info_event_content.is_live());
}

#[async_test]
async fn events_received_before_start_are_not_lost() {
    let timeline = TestTimeline::new();

    let alice_beacon_info_id: OwnedEventId = EventId::new(server_name!("dummy.server"));
    let bob_beacon_info_id: OwnedEventId = EventId::new(server_name!("dummy2.server"));

    // Alice sends her live location beacon
    timeline
        .send_beacon(&ALICE, &alice_beacon_info_id, "geo:51.5008,0.1247;u=35".to_string())
        .await;

    timeline
        .send_beacon(&ALICE, &alice_beacon_info_id, "geo:51.5008,0.1249;u=12".to_string())
        .await;

    // Bob sends his live location beacon
    timeline.send_beacon(&BOB, &bob_beacon_info_id, "geo:51.5008,0.1248;u=35".to_string()).await;

    // Alice starts her live location share
    timeline
        .send_beacon_info_with_id(
            &ALICE,
            &alice_beacon_info_id,
            create_beacon_info("Alice's Live location", 2300),
        )
        .await;

    // Bob starts his live location share
    timeline
        .send_beacon_info_with_id(
            &BOB,
            &bob_beacon_info_id,
            create_beacon_info("Bob's Live location", 2300),
        )
        .await;

    let alice_beacon_state = timeline.event_items().await[0].clone().beacon_state();
    let bob_beacon_state = timeline.event_items().await[1].clone().beacon_state();

    assert_beacon_info(
        &alice_beacon_state.beacon_info_event_content,
        &create_beacon_info("Alice's Live location", 2300),
    );
    assert_beacon(
        &alice_beacon_state.last_location.unwrap(),
        &LocationContent::new("geo:51.5008,0.1249;u=12".to_string()),
    );

    assert_beacon_info(
        &bob_beacon_state.beacon_info_event_content,
        &create_beacon_info("Bob's Live location", 2300),
    );
    assert_beacon(
        &bob_beacon_state.last_location.unwrap(),
        &LocationContent::new("geo:51.5008,0.1248;u=35".to_string()),
    );
}

fn assert_beacon_info(a: &BeaconInfoEventContent, b: &BeaconInfoEventContent) {
    assert_eq!(a.description, b.description);
    assert_eq!(a.live, b.live);
    assert_eq!(a.timeout, b.timeout);
    assert_eq!(a.asset, b.asset);
}

fn assert_beacon(a: &LocationContent, b: &LocationContent) {
    assert_eq!(a.uri, b.uri);
}

impl TestTimeline {
    async fn send_beacon_info(&self, user: &UserId, content: BeaconInfoEventContent) {
        // Send a beacon info state event to the room

        let event =
            self.event_builder.make_sync_state_event(user, "@example:localhost", content, None);
        self.handle_live_event(event).await;
    }

    async fn send_beacon(&self, user: &UserId, event_id: &EventId, geo_uri: String) {
        let owner = OwnedEventId::from(event_id);

        let beacon = BeaconEventContent::new(owner.clone(), geo_uri, None);

        let event_content = AnyMessageLikeEventContent::Beacon(beacon.clone());

        self.handle_live_message_event(user, event_content).await;
    }

    async fn send_beacon_info_with_id(
        &self,
        sender: &UserId,
        event_id: &EventId,
        content: BeaconInfoEventContent,
    ) {
        let event = self.event_builder.make_sync_state_event_with_id(
            sender,
            sender.as_str(),
            event_id,
            content,
            None,
        );
        self.handle_live_event(event).await;
    }

    async fn beacon_state(&self) -> BeaconState {
        self.event_items().await[0].clone().beacon_state()
    }
    async fn beacon_info_event(&self) -> EventTimelineItem {
        self.event_items().await[0].clone()
    }
}

impl EventTimelineItem {
    fn beacon_state(self) -> BeaconState {
        match self.content() {
            TimelineItemContent::BeaconInfoState(beacon_state) => beacon_state.clone(),
            _ => panic!("Not a beacon state"),
        }
    }
}

mod fakes {
    use std::time::Duration;

    use ruma::events::beacon_info::BeaconInfoEventContent;

    pub fn create_beacon_info(desc: &str, duration: u64) -> BeaconInfoEventContent {
        BeaconInfoEventContent::new(
            Option::from(desc.to_string()),
            Duration::from_millis(duration),
            true,
            None,
        )
    }
}
