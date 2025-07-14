use std::ops::Range;

use ruma::{
    events::{AnySyncStateEvent, room::member::MembershipState},
    serde::Raw,
};
use serde_json::{from_value as from_json_value, json};

/// Create `m.room.member` events in the given range.
///
/// The user IDs are generated as `@user_{idx}:{server}`, with `idx` being the
/// current value in `range`, so providing the same range in several method
/// calls will create events that replace the previous state.
///
/// The event IDs are generated as `$roommember_{batch}_{idx}` so it's important
/// to increment `batch` between method calls to avoid having two events with
/// the same event ID.
///
/// This method can be used as input for room builders with
/// `add_timeline_state_bulk()` or `add_state_bulk()`.
pub fn bulk_room_members<'a>(
    batch: usize,
    range: Range<usize>,
    server: &'a str,
    membership: &'a MembershipState,
) -> impl Iterator<Item = Raw<AnySyncStateEvent>> + 'a {
    range.map(move |idx| {
        let user_id = format!("@user_{idx}:{server}");
        let event_id = format!("$roommember_{batch}_{idx}");
        let ts = 151800000 + batch * 100 + idx;
        from_json_value(json!({
            "content": {
                "membership": membership,
            },
            "event_id": event_id,
            "origin_server_ts": ts,
            "sender": user_id,
            "state_key": user_id,
            "type": "m.room.member",
        }))
        .unwrap()
    })
}
