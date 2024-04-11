// Copyright 2024 The Matrix.org Foundation C.I.C.
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

pub mod create_rendezvous {
    use http::header::{CONTENT_TYPE, ETAG, EXPIRES, LAST_MODIFIED, LOCATION};
    use ruma::{
        api::{request, response, Metadata},
        metadata,
    };

    pub const METADATA: Metadata = metadata! {
        method: POST,
        rate_limited: true,
        authentication: None,
        history: {
            // TODO: Once we have a working rendezvous server, switch to the correct MSC.
            // unstable => "/_matrix/client/unstable/org.matrix.msc4108/rendezvous",
            unstable => "/_matrix/client/unstable/org.matrix.msc3886/rendezvous",
        }
    };

    #[request]
    #[derive(Default)]
    pub struct Request {}

    #[response]
    pub struct Response {
        #[ruma_api(header = LOCATION)]
        pub location: String,
        #[ruma_api(header = ETAG)]
        pub etag: String,
        #[ruma_api(header = EXPIRES)]
        pub expires: String,
        #[ruma_api(header = LAST_MODIFIED)]
        pub last_modified: String,
        #[ruma_api(header = CONTENT_TYPE)]
        pub content_type: Option<String>,
    }
}

pub mod send_rendezvous {
    use http::header::{CONTENT_TYPE, ETAG, EXPIRES, IF_MATCH, LAST_MODIFIED};
    use ruma::{
        api::{request, response, Metadata},
        metadata,
    };

    pub const METADATA: Metadata = metadata! {
        method: PUT,
        rate_limited: true,
        authentication: None,
        history: {
            unstable => "/:rendezvous_session",
        }
    };

    #[request]
    pub struct Request {
        #[ruma_api(path)]
        pub rendezvous_session: String,
        #[ruma_api(raw_body)]
        pub body: Vec<u8>,
        #[ruma_api(header = IF_MATCH)]
        pub etag: String,
        #[ruma_api(header = CONTENT_TYPE)]
        pub content_type: Option<String>,
    }

    #[response]
    pub struct Response {
        #[ruma_api(header = ETAG)]
        pub etag: String,
        #[ruma_api(header = EXPIRES)]
        pub expires: String,
        #[ruma_api(header = LAST_MODIFIED)]
        pub last_modified: String,
    }
}

pub mod receive_rendezvous {
    use http::header::{CONTENT_TYPE, ETAG, EXPIRES, IF_NONE_MATCH, LAST_MODIFIED};
    use ruma::{
        api::{request, response, Metadata},
        metadata,
    };

    pub const METADATA: Metadata = metadata! {
        method: GET,
        rate_limited: true,
        authentication: None,
        history: {
            unstable => "/:rendezvous_session",
        }
    };

    #[request]
    pub struct Request {
        #[ruma_api(path)]
        pub rendezvous_session: String,
        #[ruma_api(header = IF_NONE_MATCH)]
        pub etag: Option<String>,
    }

    #[response]
    pub struct Response {
        #[ruma_api(header = ETAG)]
        pub etag: String,
        #[ruma_api(header = EXPIRES)]
        pub expires: String,
        #[ruma_api(header = LAST_MODIFIED)]
        pub last_modified: String,
        #[ruma_api(header = CONTENT_TYPE)]
        pub content_type: Option<String>,
        #[ruma_api(raw_body)]
        pub body: Vec<u8>,
    }
}

pub mod delete_rendezvous {
    use ruma::{
        api::{request, response, Metadata},
        metadata,
    };

    pub const METADATA: Metadata = metadata! {
        method: DELETE,
        rate_limited: true,
        authentication: None,
        history: {
            unstable => "/:rendezvous_session",
        }
    };

    #[request]
    #[derive(Default)]
    pub struct Request {
        #[ruma_api(path)]
        pub rendezvous_session: String,
    }

    #[response]
    pub struct Response {}
}
