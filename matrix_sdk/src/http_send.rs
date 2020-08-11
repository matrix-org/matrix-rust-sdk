// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use std::fmt::Debug;

use matrix_sdk_common_macros::async_trait;
use reqwest::Response;

use crate::Result;

/// Abstraction around the http layer. The allows implementors to use different
/// http libraries.
#[async_trait]
pub trait HttpSend: Sync + Send + Debug {
    /// The method abstracting sending request types and receiving response types.
    ///
    /// This is called by the client every time it wants to send anything to a homeserver.
    ///
    /// # Arguments
    ///
    /// * `request` - The http request that has been converted from a ruma `Request`.
    ///
    /// # Returns
    ///
    /// A `reqwest::Response` that will be converted to a ruma `Response` in the `Client`.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use matrix_sdk::HttpSend;
    /// use matrix_sdk_common_macros::async_trait;
    /// use reqwest::Response;
    ///
    /// #[derive(Debug)]
    /// struct TestSend;
    ///
    /// impl HttpSend for TestSend {
    ///     async fn send_request(&self, request: http::Request<Vec<u8>>) -> Result<Response>
    ///     // send the request somehow
    ///     let response = send(request, method, homeserver).await?;
    ///
    ///     // reqwest can convert to and from `http::Response` types.
    ///     Ok(reqwest::Response::from(response))
    /// }
    /// }
    ///
    /// ```
    async fn send_request(&self, request: http::Request<Vec<u8>>) -> Result<Response>;
}
