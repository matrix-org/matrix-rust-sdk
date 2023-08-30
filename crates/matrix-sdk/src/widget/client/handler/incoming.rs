use std::ops::Deref;

use crate::widget::messages::{
    from_widget::{self, Action, SupportedApiVersionsResponse},
    Action as ActionType, Empty, Header, Message, MessageKind, OpenIdRequest, OpenIdResponse,
    Request as RequestBody,
};

macro_rules! generate_requests {
    ($($request:ident($request_data:ty) -> $response_data:ty),* $(,)?) => {
        #[derive(Debug, Clone)]
        pub(crate) enum Request {
            $(
                $request($request),
            )*
        }

        impl Request {
            pub(crate) fn new(header: Header, action: Action) -> Option<Self> {
                match action {
                    $(
                        from_widget::Action::$request(MessageKind::Request(r)) => {
                            Some(Self::$request($request(WithHeader::new(header, r))))
                        }
                    )*
                    _ => None,
                }
            }

            pub(crate) fn fail(self, error: impl Into<String>) -> Response {
                match self {
                    $(
                        Self::$request(r) => r.map(Err(error.into())),
                    )*
                }
            }
        }

        $(
            #[derive(Debug, Clone)]
            pub(crate) struct $request(WithHeader<RequestBody<$request_data>>);

            impl $request {
                pub(crate) fn map(self, response_data: Result<$response_data, String>) -> Response {
                    Response {
                        data: from_widget::Action::$request(self.0.data.map(response_data)),
                        header: self.0.header,
                    }
                }
            }

            impl Deref for $request {
                type Target = $request_data;

                fn deref(&self) -> &Self::Target {
                    &self.0.data.content
                }
            }
        )*
    };
}

generate_requests! {
    GetSupportedApiVersion(Empty) -> SupportedApiVersionsResponse,
    ContentLoaded(Empty) -> Empty,
    GetOpenId(OpenIdRequest) -> OpenIdResponse,
    SendEvent(from_widget::SendEventRequest) -> from_widget::SendEventResponse,
    ReadEvent(from_widget::ReadEventRequest) -> from_widget::ReadEventResponse,
}

pub(crate) type Response = WithHeader<Action>;

impl From<Response> for Message {
    fn from(response: Response) -> Self {
        Self { header: response.header, action: ActionType::FromWidget(response.data) }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct WithHeader<T> {
    header: Header,
    data: T,
}

impl<T> WithHeader<T> {
    fn new(header: Header, data: T) -> Self {
        Self { header, data }
    }
}
