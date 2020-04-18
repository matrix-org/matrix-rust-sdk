//! De-/serialization functions to and from json strings, allows the type to be used as a query string.

use serde::de::{Deserialize, Deserializer, Error as _};

use crate::events::collections::all::Event;
use crate::events::presence::PresenceEvent;
use crate::events::EventResult;

pub fn deserialize_events<'de, D>(deserializer: D) -> Result<Vec<Event>, D::Error>
where
    D: Deserializer<'de>,
{
    let mut events = vec![];
    let ev = Vec::<EventResult<Event>>::deserialize(deserializer)?;
    for event in ev {
        events.push(event.into_result().map_err(D::Error::custom)?);
    }

    Ok(events)
}

pub fn deserialize_presence<'de, D>(deserializer: D) -> Result<Vec<PresenceEvent>, D::Error>
where
    D: Deserializer<'de>,
{
    let mut events = vec![];
    let ev = Vec::<EventResult<PresenceEvent>>::deserialize(deserializer)?;
    for event in ev {
        events.push(event.into_result().map_err(D::Error::custom)?);
    }

    Ok(events)
}

#[cfg(test)]
mod test {
    use std::fs;

    use crate::events::room::member::MemberEvent;
    use crate::events::EventResult;
    use crate::models::RoomMember;

    #[test]
    fn events_and_presence_deserialization() {
        let ev_json = fs::read_to_string("./tests/data/events/member.json").unwrap();
        let ev = serde_json::from_str::<EventResult<MemberEvent>>(&ev_json)
            .unwrap()
            .into_result()
            .unwrap();
        let member = RoomMember::new(&ev);

        let member_json = serde_json::to_string(&member).unwrap();
        let mem = serde_json::from_str::<RoomMember>(&member_json).unwrap();
        assert_eq!(member, mem);
    }
}
