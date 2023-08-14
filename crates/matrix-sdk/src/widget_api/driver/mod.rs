use self::widget::Widget;
use super::{
    handler::{self, Capabilities, OpenIDState},
    messages::{
        capabilities::{EventFilter, Filter, Options},
        from_widget::{ReadEventRequest, ReadEventResponse, SendEventRequest, SendEventResponse},
        {openid, MatrixEvent},
    },
    {Error::WidgetError, Result},
};
use crate::room::MessagesOptions;
use crate::{event_handler::EventHandlerHandle, room::Joined};
use async_trait::async_trait;
use ruma::{api::client::filter::RoomEventFilter, events::AnySyncTimelineEvent, serde::Raw};
use tokio::sync::mpsc;

pub mod widget;

#[derive(Debug)]
pub struct Driver<W: Widget> {
    room: Joined,
    widget: W,
    event_handler_handle: Option<EventHandlerHandle>,
}

#[async_trait]
impl<W: Widget> handler::Client for Driver<W> {
    async fn initialise(&mut self, options: Options) -> Result<Capabilities> {
        let options = self.widget.acquire_permissions(options).await?;

        let mut capabilities = Capabilities::default();

        capabilities.event_listener = self.build_event_listener(&options.read_filter);
        capabilities.event_sender = self.build_event_sender(&options.send_filter);
        capabilities.event_reader = self.build_event_reader(&options.read_filter);
        Result::Ok(capabilities)
    }

    async fn get_openid(&self, req: openid::Request) -> OpenIDState {
        let user_id = self.room.client.user_id();
        if user_id == None {
            return OpenIDState::Resolved(None);
        }
        let user_id = user_id.unwrap();

        let request =
            ruma::api::client::account::request_openid_token::v3::Request::new(user_id.to_owned());
        let res = self.room.client.send(request, None).await;

        let state = match res {
            Err(_) => None,
            Ok(res) => Some(openid::Response {
                id: req.id,
                token: res.access_token,
                expires_in_seconds: res.expires_in.as_secs() as usize,
                server: res.matrix_server_name.to_string(),
                kind: res.token_type.to_string(),
            }),
        };
        OpenIDState::Resolved(state)
    }
}

#[derive(Debug)]
pub struct EventReader {
    room: Joined,
    filter: Vec<Filter>,
}

#[async_trait]
impl handler::EventReader for EventReader {
    fn get_filter(&self) -> &Vec<Filter> {
        &self.filter
    }

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

        match self.room.messages(options).await {
            Ok(messages) => {
                // TODO fix unwrap
                let state_events: Vec<MatrixEvent> =
                    messages.state.iter().map(|s| s.deserialize_as().unwrap()).collect();
                let timeline_events: Vec<MatrixEvent> =
                    messages.chunk.iter().map(|msg| msg.event.deserialize_as().unwrap()).collect();
                let all_messages = vec![state_events, timeline_events].concat();
                let filtered_messages = all_messages
                    .into_iter()
                    .filter(|m| {
                        let filter_fn =
                            |f: &Filter| f.allow_event(&m.event_type, &m.state_key, &m.content);
                        self.filter.iter().any(filter_fn)
                    })
                    .collect();
                Ok(ReadEventResponse { events: filtered_messages })
            }
            Err(err) => Err(WidgetError(
                format!("Could not fetch messages from homeserver: {}", err.to_string())
                    .to_string(),
            )),
        }
    }
}
#[derive(Debug)]
pub struct EventSender {
    room: Joined,
    filter: Vec<Filter>,
}

#[async_trait]
impl handler::EventSender for EventSender {
    fn get_filter(&self) -> &Vec<Filter> {
        &self.filter
    }

    async fn send(&self, req: SendEventRequest) -> Result<SendEventResponse> {
        let filter_fn = |f: &Filter| f.allow_event(&req.message_type, &req.state_key, &req.content);

        if self.filter.iter().any(filter_fn) {
            match req.state_key {
                Some(state_key) => {
                    match self
                        .room
                        .send_state_event_raw(req.content, &req.message_type, &state_key)
                        .await
                    {
                        Ok(send_res) => Ok(SendEventResponse {
                            room_id: self.room.room_id().to_string(),
                            event_id: send_res.event_id.to_string(),
                        }),
                        Err(err) => {
                            Err(WidgetError(format!("Could not send event with error: {}", err)))
                        }
                    }
                }
                None => match self.room.send_raw(req.content, &req.message_type, None).await {
                    Ok(send_res) => Ok(SendEventResponse {
                        room_id: self.room.room_id().to_string(),
                        event_id: send_res.event_id.to_string(),
                    }),
                    Err(err) => {
                        Err(WidgetError(format!("Could not send event with error: {}", err)))
                    }
                },
            }
        } else {
            Err(WidgetError(format!(
                "No capability to send event of type {} with state key {} (for room events the state key is undefined if no state key is shown the state key is \"\")",
                req.message_type, req.state_key.unwrap_or("undefined".to_string())
            )))
        }
    }
}

impl<W: Widget> Driver<W> {
    fn build_event_sender(&self, filter: &Vec<Filter>) -> Option<Box<dyn handler::EventSender>> {
        if filter.len() > 0 {
            let s: Box<dyn handler::EventSender> =
                Box::new(EventSender { room: self.room.clone(), filter: filter.clone() });
            return Some(s);
        }
        None
    }

    fn build_event_reader(&self, filter: &Vec<Filter>) -> Option<Box<dyn handler::EventReader>> {
        if filter.len() > 0 {
            Some(Box::new(EventReader { room: self.room.clone(), filter: filter.clone() }))
        } else {
            None
        }
    }

    fn build_event_listener(
        &mut self,
        filter: &Vec<Filter>,
    ) -> Option<mpsc::UnboundedReceiver<MatrixEvent>> {
        let (tx, rx) = mpsc::unbounded_channel::<MatrixEvent>();
        let filter = filter.clone();
        if filter.len() > 0 {
            let callback = move |ev: Raw<AnySyncTimelineEvent>| {
                let filter = filter.clone();
                let tx = tx.clone();
                async move {
                    match ev.deserialize_as::<MatrixEvent>() {
                        Ok(m_ev) => {
                            if (&filter).clone().iter().any(|f| {
                                f.allow_event(&m_ev.event_type, &m_ev.state_key, &m_ev.content)
                            }) {
                                let _= tx.send(m_ev).map_err(|err|eprintln!("Could not send sync matrix message to another thread: {err}"));
                            }
                        }
                        Err(err) => {
                            eprintln!("Could not parse AnySyncTimelineEvent as crate::widget_api::MatrixEvent: {err}");
                        }
                    }
                }
            };
            self.event_handler_handle = Some(self.room.add_event_handler(callback));
            return Some(rx);
        }

        None
    }
}
