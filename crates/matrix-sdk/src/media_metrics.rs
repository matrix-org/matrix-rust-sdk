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

use std::collections::{BTreeMap, HashMap};

use matrix_sdk_base::{
    deserialized_responses::TimelineEvent, event_cache::store::EventCacheStoreLockState,
};
use ruma::{
    OwnedMxcUri, OwnedRoomId,
    events::{
        AnySyncMessageLikeEvent, AnySyncTimelineEvent,
        room::{MediaSource, message::MessageType},
    },
};

use crate::{Client, Error, Room, media::MediaEventContent};

/// The type of a media item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MediaType {
    /// An image.
    Image,

    /// A video.
    Video,

    /// An audio file.
    Audio,

    /// A generic file.
    File,

    /// Any other media, such as a location preview.
    Other,
}

/// The number of media events and the size of their cached representations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MediaUsage {
    /// The number of media events.
    pub count: u64,

    /// The total size in bytes of the cached representations of the media,
    /// including thumbnails.
    pub size: u64,
}

/// Metrics about the media cached locally.
///
/// The metrics are computed from the events in the local event cache. Media
/// that is cached without a corresponding cached event is not included. The
/// size of a media item is only counted once, even if it is referenced by
/// several events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaMetrics {
    /// Metrics for all media events.
    pub total: MediaUsage,

    /// Metrics for each media type.
    pub by_type: BTreeMap<MediaType, MediaUsage>,
}

impl Client {
    /// Get metrics about the media cached locally for all rooms.
    ///
    /// The metrics are computed from the events in the local event cache. A
    /// media event whose content isn't cached yet still counts towards the
    /// number of media events, but not towards the size.
    pub async fn media_metrics(&self) -> Result<MediaMetrics, Error> {
        let room_ids: Vec<OwnedRoomId> =
            self.rooms().into_iter().map(|room| room.room_id().to_owned()).collect();

        collect_media_metrics(self, &room_ids).await
    }
}

impl Room {
    /// Get metrics about the media cached locally for this room.
    ///
    /// The metrics are computed from the events in the local event cache. A
    /// media event whose content isn't cached yet still counts towards the
    /// number of media events, but not towards the size.
    pub async fn media_metrics(&self) -> Result<MediaMetrics, Error> {
        let client = self.client();
        let room_id = self.room_id().to_owned();

        collect_media_metrics(&client, &[room_id]).await
    }
}

async fn collect_media_metrics(
    client: &Client,
    room_ids: &[OwnedRoomId],
) -> Result<MediaMetrics, Error> {
    let mut by_type: BTreeMap<MediaType, MediaUsage> = BTreeMap::new();
    let mut uris: HashMap<OwnedMxcUri, MediaType> = HashMap::new();

    {
        let state = client.event_cache_store().lock().await?;
        let store = match state {
            EventCacheStoreLockState::Clean(store) | EventCacheStoreLockState::Dirty(store) => {
                store
            }
        };

        for room_id in room_ids {
            for event_type in ["m.room.message", "m.sticker"] {
                let events = store.get_room_events(room_id, Some(event_type), None).await?;

                for event in events {
                    let Some((media_type, media_uris)) = media_of(&event) else {
                        continue;
                    };

                    by_type.entry(media_type).or_default().count += 1;

                    for uri in media_uris {
                        uris.entry(uri).or_insert(media_type);
                    }
                }
            }
        }
    }

    if !uris.is_empty() {
        let uris_to_query = uris.keys().cloned().collect::<Vec<_>>();
        let guard = client.media_store().lock().await?;

        for (uri, size) in guard.media_sizes(&uris_to_query).await? {
            if let Some(media_type) = uris.get(&uri) {
                by_type.entry(*media_type).or_default().size += size;
            }
        }
    }

    let total = MediaUsage {
        count: by_type.values().map(|usage| usage.count).sum(),
        size: by_type.values().map(|usage| usage.size).sum(),
    };

    Ok(MediaMetrics { total, by_type })
}

fn media_of(event: &TimelineEvent) -> Option<(MediaType, Vec<OwnedMxcUri>)> {
    let event = event.raw().deserialize().ok()?;

    match event {
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(message)) => {
            let content = &message.as_original()?.content;

            let (media_type, content): (MediaType, &dyn MediaEventContent) = match &content.msgtype
            {
                MessageType::Image(content) => (MediaType::Image, content),
                MessageType::Video(content) => (MediaType::Video, content),
                MessageType::Audio(content) => (MediaType::Audio, content),
                MessageType::File(content) => (MediaType::File, content),
                MessageType::Location(content) => (MediaType::Other, content),
                _ => return None,
            };

            media_uris(media_type, content)
        }

        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::Sticker(sticker)) => {
            let content = &sticker.as_original()?.content;

            media_uris(MediaType::Image, content)
        }

        _ => None,
    }
}

fn media_uris(
    media_type: MediaType,
    content: &dyn MediaEventContent,
) -> Option<(MediaType, Vec<OwnedMxcUri>)> {
    let mut uris = Vec::new();

    for source in [content.source(), content.thumbnail_source()].into_iter().flatten() {
        uris.push(match source {
            MediaSource::Plain(uri) => uri,
            MediaSource::Encrypted(file) => file.url.clone(),
        });
    }

    uris.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    uris.dedup();

    (!uris.is_empty()).then_some((media_type, uris))
}

#[cfg(test)]
mod tests {
    use matrix_sdk_base::deserialized_responses::TimelineEvent;
    use ruma::{mxc_uri, serde::Raw};
    use serde_json::json;

    use super::{MediaType, media_of};

    fn timeline_event(value: serde_json::Value) -> TimelineEvent {
        let json = serde_json::to_string(&value).unwrap();
        TimelineEvent::from_plaintext(Raw::from_json_string(json).unwrap())
    }

    #[test]
    fn media_of_image_uses_file_and_thumbnail() {
        let event = timeline_event(json!({
            "type": "m.room.message",
            "event_id": "$event",
            "sender": "@alice:example.org",
            "origin_server_ts": 0,
            "content": {
                "msgtype": "m.image",
                "body": "image.png",
                "url": "mxc://example.org/image",
                "info": {
                    "thumbnail_url": "mxc://example.org/thumbnail"
                }
            }
        }));

        let (media_type, uris) = media_of(&event).unwrap();

        assert_eq!(media_type, MediaType::Image);
        assert_eq!(
            uris,
            vec![
                mxc_uri!("mxc://example.org/image").to_owned(),
                mxc_uri!("mxc://example.org/thumbnail").to_owned(),
            ]
        );
    }

    #[test]
    fn media_of_text_is_ignored() {
        let event = timeline_event(json!({
            "type": "m.room.message",
            "event_id": "$event",
            "sender": "@alice:example.org",
            "origin_server_ts": 0,
            "content": {
                "msgtype": "m.text",
                "body": "hello"
            }
        }));

        assert!(media_of(&event).is_none());
    }
}
