use std::fmt::Debug;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

const SEND_EVENT: &str = "org.matrix.msc2762.m.send.event";
const READ_EVENT: &str = "org.matrix.msc2762.m.receive.event";
const SEND_STATE: &str = "org.matrix.msc2762.m.send.state_event";
const READ_STATE: &str = "org.matrix.msc2762.m.receive.state_event";

#[derive(Debug, Default, Clone)]
pub struct Options {
    pub screenshot: bool,

    // room events
    pub send_filter: Vec<Filter>,
    pub read_filter: Vec<Filter>,
    // state events
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

        let all_filter = vec![
            self.send_filter
                .clone()
                .into_iter()
                .map(|x| (if x.is_state_filter() { SEND_STATE } else { SEND_EVENT }, x))
                .collect::<Vec<(&str, Filter)>>(),
            self.read_filter
                .clone()
                .into_iter()
                .map(|x| (if x.is_state_filter() { READ_STATE } else { READ_EVENT }, x))
                .collect::<Vec<(&str, Filter)>>(),
        ]
        .concat();

        for (base, filter) in all_filter {
            match filter.capability_extension() {
                Ok(ext) => capability_list.push(base.to_owned() + &ext),
                Err(_) => continue,
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
        let mut capabilities = Options::default();

        for capability in capability_list {
            if capability == "m.capability.screenshot" {
                capabilities.screenshot = true;
            }
            if capability == "m.always_on_screen" {
                capabilities.always_on_screen = true;
            }
            if capability == "io.element.requires_client" {
                capabilities.requires_client = true;
            }
            if capability.starts_with(SEND_EVENT) {
                let cap_split: Vec<&str> = capability.split(":").collect();
                if cap_split.len() > 1 {
                    capabilities
                        .send_filter
                        .push(Filter::Timeline(serde_json::from_str(cap_split[1]).unwrap()));
                } else {
                    capabilities.send_filter.push(Filter::AllowAllTimeline);
                }
            }
            if capability.starts_with(READ_EVENT) {
                let cap_split: Vec<&str> = capability.split(":").collect();
                if cap_split.len() > 1 {
                    capabilities
                        .read_filter
                        .push(Filter::Timeline(serde_json::from_str(cap_split[1]).unwrap()));
                } else {
                    capabilities.read_filter.push(Filter::AllowAllTimeline);
                }
            }
            if capability.starts_with(SEND_STATE) {
                let cap_split: Vec<&str> = capability.split(":").collect();
                if cap_split.len() > 1 {
                    capabilities
                        .send_filter
                        .push(Filter::State(serde_json::from_str(cap_split[1]).unwrap()));
                } else {
                    capabilities.send_filter.push(Filter::AllowAllState);
                }
            }
            if capability.starts_with(READ_STATE) {
                let cap_split: Vec<&str> = capability.split(":").collect();
                if cap_split.len() > 1 {
                    capabilities
                        .read_filter
                        .push(Filter::State(serde_json::from_str(cap_split[1]).unwrap()));
                } else {
                    capabilities.read_filter.push(Filter::AllowAllState);
                }
            }
        }

        Ok(capabilities)
    }
}

// Event Filters
#[derive(Debug, Clone)]
pub enum Filter {
    Timeline(TimelineFilter),
    State(StateFilter),
    AllowAllTimeline,
    AllowAllState,
}
impl EventFilter for Filter {
    fn allow_event(
        &self,
        message_type: &String,
        state_key: &Option<String>,
        content: &serde_json::Value,
    ) -> bool {
        match self {
            Filter::Timeline(f) => f.allow_event(message_type, state_key, content),
            Filter::State(f) => f.allow_event(message_type, state_key, content),
            Filter::AllowAllTimeline => state_key.is_none(),
            Filter::AllowAllState => state_key.is_some(),
        }
    }
}
impl Filter {
    pub fn is_state_filter(&self) -> bool {
        match self {
            Filter::Timeline(_) | Filter::AllowAllTimeline => false,
            Filter::State(_) | Filter::AllowAllState => true,
        }
    }
    fn capability_extension(&self) -> Result<String, serde_json::Error> {
        match self {
            Filter::State(s_filter) => serde_json::to_string(s_filter),
            Filter::Timeline(t_filter) => serde_json::to_string(t_filter),
            Filter::AllowAllTimeline | Filter::AllowAllState => Ok("".to_owned()),
        }
    }
}
pub trait EventFilter {
    fn allow_event(
        &self,
        message_type: &String,
        state_key: &Option<String>,
        content: &serde_json::Value,
    ) -> bool;
}
#[derive(Debug, Default, Clone)]
pub struct TimelineFilter {
    event_type: String,
    msgtype: Option<String>,
}
impl EventFilter for TimelineFilter {
    fn allow_event(
        &self,
        message_type: &String,
        _state_key: &Option<String>,
        content: &serde_json::Value,
    ) -> bool {
        if message_type == &self.event_type {
            if let Some(msgtype) = self.msgtype.clone() {
                if message_type == "m.room.message" {
                    if content
                        .get("msgtype")
                        .unwrap_or(&serde_json::to_value("").unwrap())
                        .to_string()
                        == msgtype
                    {
                        return true;
                    }
                }
                return false;
            }
            return true;
        }
        return false;
    }
}
#[derive(Debug, Default, Clone)]
pub struct EventFilterAllowAllState {}
impl EventFilter for EventFilterAllowAllState {
    fn allow_event(
        &self,
        message_type: &String,
        state_key: &Option<String>,
        content: &serde_json::Value,
    ) -> bool {
        return state_key.is_some();
    }
}
#[derive(Debug, Default, Clone)]
pub struct EventFilterAllowAllRoom {}
impl EventFilter for EventFilterAllowAllRoom {
    fn allow_event(
        &self,
        _message_type: &String,
        state_key: &Option<String>,
        _content: &serde_json::Value,
    ) -> bool {
        return state_key.is_none();
    }
}
#[derive(Debug, Default, Clone)]
pub struct StateFilter {
    event_type: String,
    state_key: Option<String>,
}
impl EventFilter for StateFilter {
    fn allow_event(
        &self,
        message_type: &String,
        state_key: &Option<String>,
        _content: &serde_json::Value,
    ) -> bool {
        if message_type == &self.event_type {
            if let (Some(filter_key), Some(ev_key)) = (self.state_key.clone(), state_key.clone()) {
                return filter_key == ev_key;
            }
            return true;
        }
        return false;
    }
}
impl Serialize for TimelineFilter {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut string = format!("{}", self.event_type);
        if let Some(msgtype) = &self.msgtype {
            string = format!("{}#{}", string, msgtype);
        }
        serializer.serialize_str(&string)
    }
}
impl Serialize for StateFilter {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut string = format!(":{}", self.event_type);
        if let Some(state_key) = &self.state_key {
            string = format!("{}#{}", string, state_key);
        }
        serializer.serialize_str(&string)
    }
}

impl<'de> Deserialize<'de> for StateFilter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let des_string = String::deserialize(deserializer)?;
        let split: Vec<&str> = des_string.split("#").collect();
        let ev_type = split[0].to_owned();
        let mut state_key: Option<String> = None;
        if split.len() > 1 {
            state_key = Some(split[1].to_owned())
        }
        Ok(StateFilter { event_type: ev_type, state_key: state_key })
    }
}
impl<'de> Deserialize<'de> for TimelineFilter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let des_string = String::deserialize(deserializer)?;
        let split: Vec<&str> = des_string.split("#").collect();
        let ev_type = split[0].to_owned();
        let mut msgtype: Option<String> = None;
        if split.len() > 1 {
            msgtype = Some(split[1].to_owned())
        }
        Ok(TimelineFilter { event_type: ev_type, msgtype: msgtype })
    }
}
