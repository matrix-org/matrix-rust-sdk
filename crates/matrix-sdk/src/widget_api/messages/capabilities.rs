use serde::{Deserialize, Deserializer, Serialize, Serializer};


#[derive(Debug, Default)]
pub struct EventFilter {
    event_type: String,
    msgtype: Option<String>,
}

#[derive(Debug, Default)]
pub struct StateEventFilter {
    event_type: String,
    state_key: Option<String>,
}

impl Serialize for EventFilter {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut string = format!("{}", self.event_type);
        if let Some(msgtype) = &self.msgtype {
            string = format!("{}#{}", string, msgtype);
        }
        serializer.serialize_str(&string)
    }
}
impl Serialize for StateEventFilter {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut string = format!(":{}", self.event_type);
        if let Some(state_key) = &self.state_key {
            string = format!("{}#{}", string, state_key);
        }
        serializer.serialize_str(&string)
    }
}

impl<'de> Deserialize<'de> for StateEventFilter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let des_string = String::deserialize(deserializer)?;
        let split: Vec<&str> = des_string.split("#").collect();
        let ev_type = split[0].to_owned();
        let mut state_key: Option<String> = None;
        if split.len() > 1 {
            state_key = Some(split[1].to_owned())
        }
        Ok(StateEventFilter { event_type: ev_type, state_key: state_key })
    }
}
impl<'de> Deserialize<'de> for EventFilter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let des_string = String::deserialize(deserializer)?;
        let split: Vec<&str> = des_string.split("#").collect();
        let ev_type = split[0].to_owned();
        let mut msgtype: Option<String> = None;
        if split.len() > 1 {
            msgtype = Some(split[1].to_owned())
        }
        Ok(EventFilter { event_type: ev_type, msgtype: msgtype })
    }
}

#[derive(Debug, Default)]
pub struct Options {
    pub screenshot: bool,

    // room events
    pub send_room_event: Option<Vec<EventFilter>>,
    pub receive_room_event: Option<Vec<EventFilter>>,
    // state events
    pub send_state_event: Option<Vec<EventFilter>>,
    pub receive_state_event: Option<Vec<EventFilter>>,

    pub always_on_screen: bool, // "m.always_on_screen",

    pub requires_client: bool,
}

impl Serialize for Options {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut capability_list: Vec<String> = vec![];
        if self.screenshot {
            capability_list.push("m.capability.screenshot".to_owned());
        }
        if self.always_on_screen {
            capability_list.push("m.always_on_screen".to_owned());
        }
        if self.always_on_screen {
            capability_list.push("m.always_on_screen".to_owned());
        }
        if self.requires_client {
            capability_list.push("io.element.requires_client".to_owned());
        }

        if let Some(ev_filter) = &self.send_room_event {
            for event_type in ev_filter {
                let filter = serde_json::to_string(&event_type).unwrap();
                capability_list.push(format!("org.matrix.msc2762.m.send.event{}", filter));
            }
        }
        if let Some(ev_filter) = &self.receive_room_event {
            for event_type in ev_filter {
                let filter = serde_json::to_string(&event_type).unwrap();
                capability_list.push(format!("org.matrix.msc2762.m.receive.event{}", filter));
            }
        }
        if let Some(ev_filter) = &self.send_state_event {
            for event_type in ev_filter {
                let filter = serde_json::to_string(&event_type).unwrap();

                capability_list.push(format!("org.matrix.msc2762.m.send.state_event{}", filter));
            }
        }
        if let Some(ev_filter) = &self.receive_state_event {
            for event_type in ev_filter {
                let filter = serde_json::to_string(&event_type).unwrap();
                capability_list.push(format!("org.matrix.msc2762.m.receive.state_event{}", filter));
            }
        }
        capability_list.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Options {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let capability_list = Vec::<String>::deserialize(deserializer)?;
        let mut capbilities = Options::default();

        let mut send_room_event: Vec<EventFilter> = vec![];
        let mut receive_room_event: Vec<EventFilter> = vec![];
        let mut send_state_event: Vec<EventFilter> = vec![];
        let mut receive_state_event: Vec<EventFilter> = vec![];
        for capability in capability_list {
            if capability == "m.capability.screenshot" {
                capbilities.screenshot = true;
            }
            if capability == "m.always_on_screen" {
                capbilities.always_on_screen = true;
            }
            if capability == "io.element.requires_client" {
                capbilities.requires_client = true;
            }
            if capability.starts_with("org.matrix.msc2762.m.send.event") {
                let cap_split: Vec<&str> = capability.split(":").collect();
                if cap_split.len() > 1 {
                    send_room_event.push(serde_json::from_str(cap_split[1]).unwrap());
                }
            }
            if capability.starts_with("org.matrix.msc2762.m.receive.event") {
                let cap_split: Vec<&str> = capability.split(":").collect();
                if cap_split.len() > 1 {
                    receive_room_event.push(serde_json::from_str(cap_split[1]).unwrap());
                }
            }
            if capability.starts_with("org.matrix.msc2762.m.send.state_event") {
                let cap_split: Vec<&str> = capability.split(":").collect();
                if cap_split.len() > 1 {
                    send_state_event.push(serde_json::from_str(cap_split[1]).unwrap());
                }
            }
            if capability.starts_with("org.matrix.msc2762.m.receive.state_event") {
                let cap_split: Vec<&str> = capability.split(":").collect();
                if cap_split.len() > 1 {
                    receive_state_event.push(serde_json::from_str(cap_split[1]).unwrap());
                }
            }
        }
        capbilities.send_state_event =
            if send_state_event.len() > 0 { Some(send_state_event) } else { None };
        capbilities.receive_state_event =
            if receive_state_event.len() > 0 { Some(receive_state_event) } else { None };
        capbilities.receive_room_event =
            if receive_room_event.len() > 0 { Some(receive_room_event) } else { None };
        capbilities.send_room_event =
            if send_room_event.len() > 0 { Some(send_room_event) } else { None };
        Ok(capbilities)
    }
}
