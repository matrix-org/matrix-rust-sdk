use std::result::Result as StdResult;

use async_trait::async_trait;
use ruma::{
    api::client::{
        account::request_openid_token::v3::Request as OpenIDRequest, filter::RoomEventFilter,
    },
    events::AnySyncTimelineEvent,
    serde::Raw,
};
use tokio::sync::mpsc;

use self::widget::Widget;
use super::{
    handler::{
        Capabilities, Client, EventHandler as Handler, EventReader as Reader,
        EventSender as Sender, OpenIDState,
    },
    messages::{
        capabilities::{EventFilter, Filter, FilterInput, Options},
        from_widget::{ReadEventRequest, ReadEventResponse, SendEventRequest, SendEventResponse},
        {openid, MatrixEvent},
    },
    {Error, Result},
};
use crate::{event_handler::EventHandlerHandle, room::Joined, room::MessagesOptions};

pub mod widget;

#[derive(Debug)]
pub struct Driver<W> {
    room: Joined,
    widget: W,
    event_handler_handle: Option<EventHandlerHandle>,
}

impl<W> Driver<W> {
    pub fn new(room: Joined, widget: W) -> Result<Self> {
        Ok(Driver { room, widget, event_handler_handle: None })
    }
}

#[async_trait]
impl<W: Widget> Client for Driver<W> {
    async fn initialise(&mut self, options: Options) -> Result<Capabilities> {
        let options = self.widget.acquire_permissions(options).await?;

        Ok(Capabilities {
            listener: Filters::new(options.read_filter.clone())
                .map(|filter| self.setup_event_listener(filter)),
            reader: Filters::new(options.read_filter).map(|filter| {
                Box::new(EventHandler::new(self.room.clone(), filter)) as Box<dyn Reader>
            }),
            sender: Filters::new(options.send_filter).map(|filter| {
                Box::new(EventHandler::new(self.room.clone(), filter)) as Box<dyn Sender>
            }),
        })
    }

    async fn get_openid(&self, req: openid::Request) -> OpenIDState {
        let user_id = self.room.own_user_id();

        let request = OpenIDRequest::new(user_id.to_owned());
        let state = self.room.client.send(request, None).await.ok().map(|res| openid::Response {
            id: req.id,
            token: res.access_token,
            expires_in_seconds: res.expires_in.as_secs() as usize,
            server: res.matrix_server_name.to_string(),
            kind: res.token_type.to_string(),
        });

        OpenIDState::Resolved(state)
    }
}

impl<W> Driver<W> {
    fn setup_event_listener(&mut self, filter: Filters) -> mpsc::UnboundedReceiver<MatrixEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let callback = move |ev: Raw<AnySyncTimelineEvent>| {
            let (filter, tx) = (filter.clone(), tx.clone());
            if let Ok(msg) = ev.deserialize_as::<MatrixEvent>() {
                filter.allow(&msg).then(|| tx.send(msg));
            }
            async {}
        };

        self.event_handler_handle = Some(self.room.add_event_handler(callback));
        return rx;
    }
}

#[derive(Debug)]
pub struct EventHandler {
    room: Joined,
    filter: Filters,
}

impl EventHandler {
    fn new(room: Joined, filter: Filters) -> Self {
        Self { room, filter }
    }
}

#[async_trait]
impl Reader for EventHandler {
    async fn read(&self, req: ReadEventRequest) -> Result<ReadEventResponse> {
        let options = {
            let mut o = MessagesOptions::backward();
            o.limit = req.limit.into();
            o.filter = {
                let mut f = RoomEventFilter::default();
                f.types = Some(vec![req.message_type]);
                f
            };
            o
        };

        let messages = self.room.messages(options).await.map_err(|_| Error::Other)?;

        // Iterator over deserialized state messages.
        let state = messages.state.into_iter().map(|s| s.deserialize_as());

        // Iterator over deserialized timeline messages.
        let timeline = messages.chunk.into_iter().map(|m| m.event.deserialize_as());

        // Chain two iterators together and run them through the filter.
        Ok(ReadEventResponse {
            events: state
                .chain(timeline)
                .filter_map(StdResult::ok)
                .filter(|m| self.filter.allow(m))
                .collect(),
        })
    }
}

#[async_trait]
impl Sender for EventHandler {
    async fn send(&self, req: SendEventRequest) -> Result<SendEventResponse> {
        // Run the request through the filter.
        if !self.filter.allow(&req) {
            return Err(Error::InvalidPermissions);
        }

        // Send the requset based on whether the state key is set or not.
        let event_id = match req.state_key {
            Some(key) => {
                self.room
                    .send_state_event_raw(req.content, &req.message_type, &key)
                    .await
                    .map_err(|_| Error::Other)?
                    .event_id
            }
            None => {
                self.room
                    .send_raw(req.content, &req.message_type, None)
                    .await
                    .map_err(|_| Error::Other)?
                    .event_id
            }
        };

        // Return the response.
        Ok(SendEventResponse {
            room_id: self.room.room_id().to_string(),
            event_id: event_id.to_string(),
        })
    }
}

impl Handler for EventHandler {
    fn filters(&self) -> &[Filter] {
        &self.filter.filters
    }
}

#[derive(Debug, Clone)]
struct Filters {
    filters: Vec<Filter>,
}

impl Filters {
    fn new(filters: Vec<Filter>) -> Option<Self> {
        (!filters.is_empty()).then(|| Self { filters })
    }

    fn allow<'a>(&self, input: impl Into<FilterInput<'a>>) -> bool {
        let input = input.into();
        self.filters.iter().any(|f| f.allow(input.clone()))
    }
}
