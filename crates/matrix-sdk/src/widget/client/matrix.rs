use matrix_sdk_base::deserialized_responses::RawAnySyncOrStrippedState;
use ruma::{
    api::client::{
        account::request_openid_token::v3::Request as MatrixOpenIdRequest, filter::RoomEventFilter,
    },
    assign,
    events::{AnySyncTimelineEvent, AnyTimelineEvent},
    serde::Raw,
};
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

use super::handler::{Capabilities, Error, OpenIdDecision, OpenIdStatus, Result};
use crate::{
    event_handler::EventHandlerDropGuard,
    room::{MessagesOptions, Room},
    widget::{
        filter::{EventFilter, MatrixEventFilterInput},
        messages::{
            from_widget::{
                ReadEventRequest, ReadEventResponse, SendEventRequest, SendEventResponse,
                StateKeySelector,
            },
            openid::OpenIdState,
        },
        Permissions, PermissionsProvider,
    },
};

fn attach_room_id(raw_ev: &Raw<AnySyncTimelineEvent>, room_id: &str) -> Raw<AnyTimelineEvent> {
    // deserialize should be possible if Raw<AnySyncTimelineEvent> is possible
    let mut ev_value = raw_ev.deserialize_as::<serde_json::Value>().unwrap();
    let ev_obj = ev_value.as_object_mut().unwrap();
    ev_obj.insert("room_id".to_owned(), room_id.to_owned().into());
    let ev_with_room_id = serde_json::from_value::<Raw<AnyTimelineEvent>>(ev_value).unwrap();
    info!("final Event: {}", ev_with_room_id.json());
    ev_with_room_id
}

#[derive(Debug)]
pub(crate) struct Driver<T> {
    /// The room this driver is attached to.
    ///
    /// Expected to be a room the user is a member of (not a room in invited or
    /// left state).
    room: Room,
    permissions_provider: T,
    event_handler_handle: Option<EventHandlerDropGuard>,
}

impl<T> Driver<T> {
    pub(crate) fn new(room: Room, permissions_provider: T) -> Self {
        Self { room, permissions_provider, event_handler_handle: None }
    }

    pub(crate) async fn initialize(&mut self, permissions: Permissions) -> Capabilities
    where
        T: PermissionsProvider,
    {
        let permissions = self.permissions_provider.acquire_permissions(permissions).await;

        Capabilities {
            listener: Filters::new(permissions.read.clone())
                .map(|filters| self.setup_matrix_event_handler(filters)),
            reader: Filters::new(permissions.read)
                .map(|filters| EventServerProxy::new(self.room.clone(), filters)),
            sender: Filters::new(permissions.send)
                .map(|filters| EventServerProxy::new(self.room.clone(), filters)),
        }
    }

    pub(crate) fn get_openid(&self, request_id: String) -> OpenIdStatus {
        let user_id = self.room.own_user_id().to_owned();
        let client = self.room.client.clone();
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let _ = tx.send(
                client
                    .send(MatrixOpenIdRequest::new(user_id), None)
                    .await
                    .map(|res| OpenIdDecision::Allowed(OpenIdState::new(request_id, res)))
                    .unwrap_or(OpenIdDecision::Blocked),
            );
        });

        // TODO: getting an OpenId token generally has multiple phases as per the JS
        // implementation of the `ClientWidgetAPI`, e.g. the `MatrixDriver`
        // would normally have some state, so that if a token is requested
        // multiple times, it may return/resolve the token right away.
        // Currently, we assume that we always request a new token. Fix it later.
        OpenIdStatus::Pending(rx)
    }

    fn setup_matrix_event_handler(
        &mut self,
        filter: Filters,
    ) -> mpsc::UnboundedReceiver<Raw<AnyTimelineEvent>> {
        let (tx, rx) = mpsc::unbounded_channel();
        let room_id = self.room.room_id().as_str().to_owned();
        let callback = move |raw_ev: Raw<AnySyncTimelineEvent>| {
            let (filter, tx) = (filter.clone(), tx.clone());
            if let Ok(ev) = raw_ev.deserialize_as::<MatrixEventFilterInput>() {
                filter.any_matches(&ev).then(|| {
                    info!("received event for room: {}", room_id.clone().as_str());
                    tx.send(attach_room_id(&raw_ev, &room_id))
                });
            }
            async {}
        };

        let handle = self.room.add_event_handler(callback);
        let drop_guard = self.room.client().event_handler_drop_guard(handle);
        self.event_handler_handle = Some(drop_guard);
        rx
    }
}

// TODO: Should this be two types? (one for reading, one for sending)
#[derive(Debug)]
pub struct EventServerProxy {
    room: Room,
    filters: Filters,
}

impl EventServerProxy {
    fn new(room: Room, filter: Filters) -> Self {
        Self { room, filters: filter }
    }

    pub(crate) async fn read(&self, req: ReadEventRequest) -> Result<ReadEventResponse> {
        let limit = req.limit.unwrap_or(match req.state_key {
            Some(..) => 100, // Default state events limit.
            None => 100,     // Default message-like events limit.
        });
        let event_type = req.event_type.to_string();

        let pack_state_events =
            |events: Result<Vec<RawAnySyncOrStrippedState>>| -> Result<ReadEventResponse> {
                let event_iter = events.unwrap().into_iter().filter_map(|ev| match ev {
                    RawAnySyncOrStrippedState::Sync(raw) => Some(attach_room_id(
                        &raw.cast::<AnySyncTimelineEvent>(),
                        self.room.room_id().as_str(),
                    )),
                    RawAnySyncOrStrippedState::Stripped(_) => None,
                });
                if let Some(limit) = req.limit {
                    return Ok(ReadEventResponse {
                        events: event_iter.take(limit.try_into().unwrap_or(usize::MAX)).collect(),
                    });
                }

                Ok(ReadEventResponse { events: event_iter.collect() })
            };

        match req.state_key {
            Some(state_key) => match state_key {
                StateKeySelector::Any(_) => {
                    let events = self.room.get_state_events(event_type.into()).await;
                    pack_state_events(events.map_err(Error::other))
                }
                StateKeySelector::Key(state_key) => {
                    let events = self
                        .room
                        .get_state_events_for_keys(event_type.into(), &[state_key.as_str()])
                        .await;
                    pack_state_events(events.map_err(Error::other))
                }
            },
            None => {
                let options = assign!(MessagesOptions::backward(), {
                    limit: limit.into(),
                    filter: assign!(RoomEventFilter::default(), {
                        types: Some(vec![req.event_type.to_string()])
                    })
                });
                // get message like events
                let messages = self.room.messages(options).await.map_err(Error::other)?;

                // Filter the timeline events.
                let events = messages
                    .chunk
                    .into_iter()
                    .map(|ev| ev.event.cast())
                    // TODO: Log events that failed to decrypt?
                    .filter(|raw| match raw.deserialize_as() {
                        Ok(de_helper) => self.filters.any_matches(&de_helper),
                        Err(e) => {
                            warn!("Failed to deserialize timeline event: {e}");
                            false
                        }
                    })
                    .collect();
                Ok(ReadEventResponse { events })
            }
        }
        // There may be an additional state key filter depending on the
        // `req.state_key`.
    }

    pub(crate) async fn send(&self, req: SendEventRequest) -> Result<SendEventResponse> {
        let de_helper = MatrixEventFilterInput::from_send_event_request(req.clone());

        // Run the request through the filter.
        if !self.filters.any_matches(&de_helper) {
            return Err(Error::custom("Message not allowed by filter"));
        }

        // Send the request based on whether the state key is set or not.
        //
        // TODO: not sure about the `*_raw` methods here, same goes for
        // the `MatrixEvent`. I feel like stronger types would suit better here,
        // but that's how it was originally implemented by @toger5, clarify it
        // later once @jplatte reviews it.
        let event_id = match req.state_key {
            Some(state_key) => {
                self.room
                    .send_state_event_raw(req.content, &req.event_type.to_string(), &state_key)
                    .await
                    .map_err(Error::other)?
                    .event_id
            }
            None => {
                self.room
                    .send_raw(req.content, &req.event_type.to_string(), None)
                    .await
                    .map_err(Error::other)?
                    .event_id
            }
        };

        Ok(SendEventResponse {
            room_id: self.room.room_id().to_string(),
            event_id: event_id.to_string(),
        })
    }

    pub(crate) fn filters(&self) -> &[EventFilter] {
        &self.filters.filters
    }
}

#[derive(Debug, Clone)]
struct Filters {
    filters: Vec<EventFilter>,
}

impl Filters {
    fn new(filters: Vec<EventFilter>) -> Option<Self> {
        (!filters.is_empty()).then_some(Self { filters })
    }

    fn any_matches(&self, event: &MatrixEventFilterInput) -> bool {
        self.filters.iter().any(|f| f.matches(event))
    }
}
