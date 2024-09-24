use ruma::{events::location::LocationContent, MilliSecondsSinceUnixEpoch, OwnedUserId};

/// Details of the last known location beacon.
#[derive(Clone, Debug)]
pub struct LastLocation {
    /// The most recent location content of the user.
    pub location: LocationContent,
    /// The timestamp of when the location was updated
    pub ts: MilliSecondsSinceUnixEpoch,
}

/// Details of a users live location share.
#[derive(Clone, Debug)]
pub struct LiveLocationShare {
    /// The user's last known location.
    pub last_location: LastLocation,
    // /// Information about the associated beacon event (currently commented out).
    // pub beacon_info: BeaconInfoEventContent,
    /// The user ID of the person sharing their live location.
    pub user_id: OwnedUserId,
}
