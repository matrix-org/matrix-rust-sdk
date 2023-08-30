use crate::widget::messages::{
    to_widget::{Action, CapabilitiesResponse, CapabilitiesUpdatedRequest},
    Empty, MessageKind as Kind, OpenIdResponse,
};

pub(crate) type Response<T> = Result<T, String>;

pub(crate) trait Request: Sized + Send + Sync + 'static {
    type Response;

    // TODO: Nothing stops the implementor to to generate an `Action` that is
    // `Action(Kind::Response)` inside an `into_action()` implementation which is
    // bad, because we **actually** want to enforce that only an
    // `Action(Kind::Request)` could be used here. Define a special trait and
    // newtype for each possible outgoing requset and further restrict the types
    // that we can use. The response must match the action defined above. We can
    // further restrict types here for more compile time safety. This would also
    // eliminate a boilerplate copy-paste here and/or will allow to create a
    // more elegant macro for it.
    fn into_action(self) -> Action;
    fn extract_response(reply: Action) -> Option<Response<Self::Response>>;
}

pub(crate) struct CapabilitiesRequest;
impl Request for CapabilitiesRequest {
    type Response = CapabilitiesResponse;

    fn into_action(self) -> Action {
        Action::CapabilitiesRequest(Kind::empty())
    }

    fn extract_response(reply: Action) -> Option<Response<Self::Response>> {
        match reply {
            Action::CapabilitiesRequest(Kind::Response(r)) => Some(r.response()),
            _ => None,
        }
    }
}

pub(crate) struct CapabilitiesUpdate(pub(crate) CapabilitiesUpdatedRequest);
impl Request for CapabilitiesUpdate {
    type Response = Empty;

    fn into_action(self) -> Action {
        Action::CapabilitiesUpdate(Kind::request(self.0))
    }

    fn extract_response(reply: Action) -> Option<Response<Self::Response>> {
        match reply {
            Action::CapabilitiesUpdate(Kind::Response(r)) => Some(r.response()),
            _ => None,
        }
    }
}

pub(crate) struct OpenIdUpdated(pub(crate) OpenIdResponse);
impl Request for OpenIdUpdated {
    type Response = Empty;

    fn into_action(self) -> Action {
        Action::OpenIdCredentialsUpdate(Kind::request(self.0))
    }

    fn extract_response(reply: Action) -> Option<Response<Self::Response>> {
        match reply {
            Action::OpenIdCredentialsUpdate(Kind::Response(r)) => Some(r.response()),
            _ => None,
        }
    }
}
