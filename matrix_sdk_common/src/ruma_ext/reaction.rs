use crate::identifiers::EventId;

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "rel_type")]
pub enum ReactionEventContent {
    #[serde(rename = "m.annotation")]
    Annotation {
        /// The event this reaction relates to.
        event_id: EventId,
        /// The displayable content of the reaction.
        key: String,
    },
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ExtraReactionEventContent {
    /// The actual event content is nested within the field with an event type as it's
    /// JSON field name "m.relates_to".
    #[serde(rename = "m.relates_to")]
    pub relates_to: ReactionEventContent,
}
