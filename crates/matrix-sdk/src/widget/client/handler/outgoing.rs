//! Ougoing requests (client -> widget).

use crate::widget::messages::{
    openid::OpenIdResponse,
    to_widget::{Action, CapabilitiesResponse, CapabilitiesUpdatedRequest},
    Empty, MessageKind as Kind,
};

pub(crate) type Response<T> = Result<T, String>;

// TODO: This trait could be improved and restricted even more by making sure
// that `into_action()` only allows types that e.g. implement `T: AsRequest` and
// `extract_response` only accept `T: AsResponse`. Though for this both such
// traits must be introduced and implemented.
pub(crate) trait Request: Sized + Send + Sync + 'static {
    type Response;

    fn into_action(self) -> Action;
    fn extract_response(reply: Action) -> Option<Response<Self::Response>>;
}

macro_rules! generate_requests {
    ($($request:ident($request_data:ty) -> $response_data:ty),* $(,)?) => {
        $(
            #[derive(Debug, Clone)]
            pub(crate) struct $request($request_data);

            impl $request {
                pub(crate) fn new(data: $request_data) -> Self {
                    Self(data)
                }
            }

            impl Request for $request {
                type Response = $response_data;

                fn into_action(self) -> Action {
                    Action::$request(Kind::request(self.0))
                }

                fn extract_response(reply: Action) -> Option<Response<Self::Response>> {
                    match reply {
                        Action::$request(Kind::Response(r)) => Some(r.response()),
                        _ => None,
                    }
                }
            }
        )*
    };
}

generate_requests! {
    CapabilitiesRequest(Empty) -> CapabilitiesResponse,
    CapabilitiesUpdate(CapabilitiesUpdatedRequest) -> Empty,
    OpenIdCredentialsUpdate(OpenIdResponse) -> Empty,
    SendEvent(serde_json::Value) -> Empty,
}
