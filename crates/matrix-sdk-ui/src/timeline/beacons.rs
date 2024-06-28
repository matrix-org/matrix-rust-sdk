//! This module handles rendering of MSC3489 live location sharing in the
//! timeline.

use std::collections::HashMap;

use ruma::{
    events::{
        beacon::BeaconEventContent, beacon_info::BeaconInfoEventContent, location::LocationContent,
        FullStateEventContent,
    },
    EventId, OwnedEventId, OwnedUserId,
};

/// Holds the state of a beacon_info.
///
/// This struct should be created for each beacon_info event handled and then
/// updated whenever handling any beacon event that relates to the same
/// beacon_info event.
#[derive(Clone, Debug)]
pub struct BeaconState {
    pub(super) beacon_info_event_content: BeaconInfoEventContent,
    pub(super) last_location: Option<LocationContent>,
    pub(super) user_id: OwnedUserId,
}

impl BeaconState {
    pub(super) fn new(
        content: FullStateEventContent<BeaconInfoEventContent>,
        user_id: OwnedUserId,
    ) -> Self {
        match &content {
            FullStateEventContent::Original { content, .. } => BeaconState {
                beacon_info_event_content: content.clone(),
                last_location: None,
                user_id,
            },
            FullStateEventContent::Redacted(_) => {
                todo!("How should this be handled?")
            }
        }
    }

    /// Update the state with the last known associated beacon info.
    ///
    /// Used when a new beacon_info event is sent with the live field set
    /// to false.
    pub(super) fn update_beacon_info(&self, content: &BeaconInfoEventContent) -> Self {
        let mut clone = self.clone();
        clone.beacon_info_event_content = content.clone();
        clone
    }

    /// Update the state with the last known associated beacon location.
    pub(super) fn update_beacon(&self, content: &BeaconEventContent) -> Self {
        let mut clone = self.clone();
        clone.last_location = Some(content.location.clone());
        clone
    }

    /// Get the last known beacon location.
    pub fn last_location(&self) -> Option<LocationContent> {
        self.last_location.clone()
    }

    /// Get the user_id of the user who sent the beacon_info event.
    pub fn user_id(&self) -> OwnedUserId {
        self.user_id.clone()
    }
}

impl From<BeaconState> for BeaconInfoEventContent {
    fn from(value: BeaconState) -> Self {
        BeaconInfoEventContent::new(
            value.beacon_info_event_content.description.clone(),
            value.beacon_info_event_content.timeout.clone(),
            value.beacon_info_event_content.live.clone(),
            None,
        )
    }
}

/// Acts as a cache for beacons before their beacon_infos have been handled.
#[derive(Clone, Debug, Default)]
pub(super) struct BeaconPendingEvents {
    pub(super) pending_beacons: HashMap<OwnedEventId, BeaconEventContent>,
}

impl BeaconPendingEvents {
    pub(super) fn add_beacon(&mut self, start_id: &EventId, content: &BeaconEventContent) {
        self.pending_beacons.insert(start_id.to_owned(), content.clone());
    }
    pub(super) fn apply(&mut self, beacon_info_event_id: &EventId, beacon_state: &mut BeaconState) {
        if let Some(newest_beacon) = self.pending_beacons.get_mut(beacon_info_event_id) {
            beacon_state.last_location = Some(newest_beacon.location.clone());
        }
    }
}
