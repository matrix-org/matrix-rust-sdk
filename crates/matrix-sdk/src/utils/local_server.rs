// Copyright 2025 The Matrix.org Foundation C.I.C.
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

//! A server that binds to a random port on localhost and waits for a `GET` HTTP
//! request.
//!
//! # Example
//!
//! ```no_run
//! # async {
//! use matrix_sdk::utils::local_server::LocalServerBuilder;
//! # let open_uri = |uri: url::Url| {};
//! # let parse_query_string = |query: &str| {};
//!
//! let (uri, server_handle) = LocalServerBuilder::new().spawn().await?;
//!
//! open_uri(uri);
//!
//! if let Some(query_string) = server_handle.await {
//!     parse_query_string(&query_string);
//! }
//!
//! # anyhow::Ok(()) };
//! ```

use std::{
    convert::Infallible,
    fmt,
    future::IntoFuture,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    ops::{Deref, Range},
    sync::Arc,
};

use axum::{body::Body, response::IntoResponse, routing::any_service};
use http::{HeaderValue, Method, Request, StatusCode, header};
use matrix_sdk_base::{boxed_into_future, locks::Mutex};
use matrix_sdk_common::executor::spawn;
use rand::{Rng, thread_rng};
use tokio::{net::TcpListener, sync::oneshot};
use tower::service_fn;
use url::Url;

/// The default range of ports the server will try to bind to randomly.
const DEFAULT_PORT_RANGE: Range<u16> = 20000..30000;
/// The default number of times the server will try to bind to a random
/// port.
const DEFAULT_BIND_TRIES: u8 = 10;

/// Builder for a server that binds on a random port on localhost and waits for
/// a `GET` HTTP request.
///
/// The server is spawned when calling [`LocalServerBuilder::spawn()`].
///
/// The query string of the URI where the end-user is redirected is available by
/// `.await`ing the [`LocalServerRedirectHandle`], in case it should receive
/// parameters.
///
/// The server is shutdown when [`LocalServerRedirectHandle`] is dropped. It can
/// also be shutdown manually with a [`LocalServerShutdownHandle`] obtained from
/// [`LocalServerRedirectHandle::shutdown_handle()`].
#[derive(Debug, Default, Clone)]
pub struct LocalServerBuilder {
    ip_address: Option<LocalServerIpAddress>,
    port_range: Option<Range<u16>>,
    bind_tries: Option<u8>,
    response: Option<LocalServerResponse>,
}

impl LocalServerBuilder {
    /// Construct a [`LocalServerBuilder`] using the default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the IP address that the server should to bind to.
    ///
    /// Defaults to [`LocalServerIpAddress::LocalhostAny`].
    pub fn ip_address(mut self, ip_address: LocalServerIpAddress) -> Self {
        self.ip_address = Some(ip_address);
        self
    }

    /// The default range of ports the server will try to bind to randomly.
    ///
    /// Care should be taken not to bind to a [port blocked by browsers].
    ///
    /// Defaults to ports in the `20000..30000` range.
    ///
    /// [port blocked by browsers]: https://fetch.spec.whatwg.org/#port-blocking
    pub fn port_range(mut self, range: Range<u16>) -> Self {
        self.port_range = Some(range);
        self
    }

    /// The number of times the server will try to bind to a random port on
    /// localhost.
    ///
    /// Since random ports might already be taken, this setting allows to try to
    /// bind to several random ports before giving up.
    ///
    /// Defaults to `10`.
    pub fn bind_tries(mut self, tries: u8) -> Self {
        self.bind_tries = Some(tries);
        self
    }

    /// Set the content of the page that the end user will see when they a
    /// redirected to the server's URI.
    ///
    /// Defaults to a plain text page with a generic message.
    pub fn response(mut self, response: LocalServerResponse) -> Self {
        self.response = Some(response);
        self
    }

    /// Spawn the server.
    ///
    /// Returns the [`Url`] where the server is listening, and a
    /// [`LocalServerRedirectHandle`] to `await` the redirect or to shutdown
    /// the server. Returns an error if the server could not be bound to a port
    /// on localhost.
    pub async fn spawn(self) -> Result<(Url, LocalServerRedirectHandle), io::Error> {
        let Self { ip_address, port_range, bind_tries, response } = self;

        // Bind a TCP listener to a random port.
        let listener = {
            let ip_addresses = ip_address.unwrap_or_default().ip_addresses();
            let port_range = port_range.unwrap_or(DEFAULT_PORT_RANGE);
            let bind_tries = bind_tries.unwrap_or(DEFAULT_BIND_TRIES);
            let mut n = 0u8;

            loop {
                let port = thread_rng().gen_range(port_range.clone());
                let socket_addresses =
                    ip_addresses.iter().map(|ip| SocketAddr::new(*ip, port)).collect::<Vec<_>>();

                match TcpListener::bind(socket_addresses.as_slice()).await {
                    Ok(l) => {
                        break l;
                    }
                    Err(_) if n < bind_tries => {
                        n += 1;
                    }
                    Err(e) => {
                        return Err(e);
                    }
                }
            }
        };

        let socket_address =
            listener.local_addr().expect("bound TCP listener should have an address");
        let uri = Url::parse(&format!("http://{socket_address}/"))
            .expect("socket address should parse as a URI host");

        // The channel used to shutdown the server when we are done with it.
        let (shutdown_signal_sender, shutdown_signal_receiver) = oneshot::channel::<()>();
        // The channel used to transmit the data received a the redirect URL.
        let (data_sender, data_receiver) = oneshot::channel::<Option<QueryString>>();
        let data_sender_mutex = Arc::new(Mutex::new(Some(data_sender)));

        // Set up the server.
        let router = any_service(service_fn(move |request: Request<_>| {
            let data_sender_mutex = data_sender_mutex.clone();
            let response = response.clone();

            async move {
                // Reject methods others than HEAD or GET.
                if request.method() != Method::HEAD && request.method() != Method::GET {
                    return Ok::<_, Infallible>(StatusCode::METHOD_NOT_ALLOWED.into_response());
                }

                // We only need to get the first response so we consume the transmitter the
                // first time.
                if let Some(data_sender) = data_sender_mutex.lock().take() {
                    let _ =
                        data_sender.send(request.uri().query().map(|s| QueryString(s.to_owned())));
                }

                Ok(response.unwrap_or_default().into_response())
            }
        }));

        let server = axum::serve(listener, router)
            .with_graceful_shutdown(async {
                shutdown_signal_receiver.await.ok();
            })
            .into_future();

        // Spawn the server.
        spawn(server);

        Ok((
            uri,
            LocalServerRedirectHandle {
                data_receiver: Some(data_receiver),
                shutdown_signal_sender: Arc::new(Mutex::new(Some(shutdown_signal_sender))),
            },
        ))
    }
}

/// A handle to wait for the end-user to be redirected to a server spawned by
/// [`LocalServerBuilder`].
///
/// Constructed with [`LocalServerBuilder::spawn()`].
///
/// `await`ing this type returns the query string of the URI where the end-user
/// is redirected.
///
/// The server is shutdown when this handle is dropped. It can also be shutdown
/// manually with a [`LocalServerShutdownHandle`] obtained from
/// [`LocalServerRedirectHandle::shutdown_handle()`].
#[allow(missing_debug_implementations)]
pub struct LocalServerRedirectHandle {
    /// The receiver to receive the query string.
    data_receiver: Option<oneshot::Receiver<Option<QueryString>>>,

    /// The sender used to send the signal to shutdown the server.
    shutdown_signal_sender: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl LocalServerRedirectHandle {
    /// Get a [`LocalServerShutdownHandle`].
    pub fn shutdown_handle(&self) -> LocalServerShutdownHandle {
        LocalServerShutdownHandle(self.shutdown_signal_sender.clone())
    }
}

impl Drop for LocalServerRedirectHandle {
    fn drop(&mut self) {
        if let Some(sender) = self.shutdown_signal_sender.lock().take() {
            let _ = sender.send(());
        }
    }
}

impl IntoFuture for LocalServerRedirectHandle {
    type Output = Option<QueryString>;
    boxed_into_future!();

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let mut this = self;

            let data_receiver =
                this.data_receiver.take().expect("data receiver is set during construction");
            data_receiver.await.ok().flatten()
        })
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for LocalServerRedirectHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalServerRedirectHandle").finish_non_exhaustive()
    }
}

/// A handle to shutdown a server spawned by [`LocalServerBuilder`].
///
/// Constructed with [`LocalServerRedirectHandle::shutdown_handle()`].
///
/// Calling [`LocalServerShutdownHandle::shutdown()`] will shutdown the
/// server before the end-user is redirected to it.
#[derive(Clone)]
#[allow(missing_debug_implementations)]
pub struct LocalServerShutdownHandle(Arc<Mutex<Option<oneshot::Sender<()>>>>);

impl LocalServerShutdownHandle {
    /// Shutdown the local redirect server.
    ///
    /// This is a noop if the server was already shutdown.
    pub fn shutdown(self) {
        if let Some(sender) = self.0.lock().take() {
            let _ = sender.send(());
        }
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for LocalServerShutdownHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalServerShutdownHandle").finish_non_exhaustive()
    }
}

/// The IP address that we want the [`LocalServerBuilder`] to bind to.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LocalServerIpAddress {
    /// Bind to the localhost IPv4 address, `127.0.0.1`.
    Localhostv4,

    /// Bind to the localhost IPv6 address, `::1`,
    Localhostv6,

    /// Bind to localhost on an IPv6 or an IPv4 address.
    ///
    /// This is the default value.
    #[default]
    LocalhostAny,

    /// Bind to a custom IP Address.
    Custom(IpAddr),
}

impl LocalServerIpAddress {
    /// Get the addresses to bind to.
    fn ip_addresses(self) -> Vec<IpAddr> {
        match self {
            Self::Localhostv4 => vec![Ipv4Addr::LOCALHOST.into()],
            Self::Localhostv6 => vec![Ipv6Addr::LOCALHOST.into()],
            Self::LocalhostAny => vec![Ipv4Addr::LOCALHOST.into(), Ipv6Addr::LOCALHOST.into()],
            Self::Custom(ip) => vec![ip],
        }
    }
}

/// The content that the end user will see when they a redirected to the
/// local server's URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalServerResponse {
    /// A plain text body.
    PlainText(String),

    /// An HTML body.
    Html(String),
}

impl LocalServerResponse {
    /// Convert this body into an HTTP response.
    fn into_response(self) -> http::Response<Body> {
        let (content_type, body) = match self {
            Self::PlainText(body) => {
                (HeaderValue::from_static(mime::TEXT_PLAIN_UTF_8.as_ref()), body)
            }
            Self::Html(body) => (HeaderValue::from_static(mime::TEXT_HTML_UTF_8.as_ref()), body),
        };

        let mut response = Body::from(body).into_response();
        response.headers_mut().insert(header::CONTENT_TYPE, content_type);

        response
    }
}

impl Default for LocalServerResponse {
    fn default() -> Self {
        LocalServerResponse::PlainText(
            "The authorization step is complete. You can close this page.".to_owned(),
        )
    }
}

/// A query string from a URI.
///
/// This is just a wrapper to have a strong type around a `String`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryString(pub String);

impl AsRef<str> for QueryString {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Deref for QueryString {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use http::header;
    use matrix_sdk_test::async_test;

    use crate::{
        assert_let_timeout,
        utils::local_server::{LocalServerBuilder, LocalServerIpAddress, LocalServerResponse},
    };

    #[async_test]
    async fn test_local_server_builder_no_query() {
        let (uri, server_handle) = LocalServerBuilder::new().spawn().await.unwrap();

        let http_client = reqwest::Client::new();
        http_client.get(uri.as_str()).send().await.unwrap();

        assert_let_timeout!(None = server_handle);
    }

    #[async_test]
    async fn test_local_server_builder_with_query() {
        let (mut uri, server_handle) = LocalServerBuilder::new().spawn().await.unwrap();
        uri.set_query(Some("foo=bar"));

        let http_client = reqwest::Client::new();
        http_client.get(uri.as_str()).send().await.unwrap();

        assert_let_timeout!(Some(query) = server_handle);
        assert_eq!(query.0, "foo=bar");
    }

    #[async_test]
    async fn test_local_server_builder_with_ipv4_and_port() {
        let (mut uri, server_handle) = LocalServerBuilder::new()
            .ip_address(LocalServerIpAddress::Localhostv4)
            .port_range(3000..3001)
            .bind_tries(1)
            .spawn()
            .await
            .unwrap();
        uri.set_query(Some("foo=bar"));

        assert_eq!(uri.host_str(), Some("127.0.0.1"));
        assert_eq!(uri.port(), Some(3000));

        let http_client = reqwest::Client::new();
        http_client.get(uri.as_str()).send().await.unwrap();

        assert_let_timeout!(Some(query) = server_handle);
        assert_eq!(query.0, "foo=bar");
    }

    #[async_test]
    async fn test_local_server_builder_with_ipv6_and_port() {
        let (mut uri, server_handle) = LocalServerBuilder::new()
            .ip_address(LocalServerIpAddress::Localhostv6)
            .port_range(10000..10001)
            .bind_tries(1)
            .spawn()
            .await
            .unwrap();
        uri.set_query(Some("foo=bar"));

        assert_eq!(uri.host_str(), Some("[::1]"));
        assert_eq!(uri.port(), Some(10000));

        let http_client = reqwest::Client::new();
        http_client.get(uri.as_str()).send().await.unwrap();

        assert_let_timeout!(Some(query) = server_handle);
        assert_eq!(query.0, "foo=bar");
    }

    #[async_test]
    async fn test_local_server_builder_with_custom_ip_and_port() {
        let (mut uri, server_handle) = LocalServerBuilder::new()
            .ip_address(LocalServerIpAddress::Custom(Ipv4Addr::new(127, 0, 0, 1).into()))
            .port_range(10040..10041)
            .bind_tries(1)
            .spawn()
            .await
            .unwrap();
        uri.set_query(Some("foo=bar"));

        assert_eq!(uri.host_str(), Some("127.0.0.1"));
        assert_eq!(uri.port(), Some(10040));

        let http_client = reqwest::Client::new();
        http_client.get(uri.as_str()).send().await.unwrap();

        assert_let_timeout!(Some(query) = server_handle);
        assert_eq!(query.0, "foo=bar");
    }

    #[async_test]
    async fn test_local_server_builder_with_custom_plain_text_response() {
        let text = "Hello world!";
        let (mut uri, server_handle) = LocalServerBuilder::new()
            .response(LocalServerResponse::PlainText(text.to_owned()))
            .spawn()
            .await
            .unwrap();
        uri.set_query(Some("foo=bar"));

        let http_client = reqwest::Client::new();
        let response = http_client.get(uri.as_str()).send().await.unwrap();

        let content_type = response.headers().get(header::CONTENT_TYPE).unwrap();
        assert_eq!(content_type, "text/plain; charset=utf-8");
        assert_eq!(response.text().await.unwrap(), text);

        assert_let_timeout!(Some(query) = server_handle);
        assert_eq!(query.0, "foo=bar");
    }

    #[async_test]
    async fn test_local_server_builder_with_custom_html_response() {
        let html = "<html><body><h1>Hello world!</h1></body></html>";
        let (mut uri, server_handle) = LocalServerBuilder::new()
            .response(LocalServerResponse::Html(html.to_owned()))
            .spawn()
            .await
            .unwrap();
        uri.set_query(Some("foo=bar"));

        let http_client = reqwest::Client::new();
        let response = http_client.get(uri.as_str()).send().await.unwrap();

        let content_type = response.headers().get(header::CONTENT_TYPE).unwrap();
        assert_eq!(content_type, "text/html; charset=utf-8");
        assert_eq!(response.text().await.unwrap(), html);

        assert_let_timeout!(Some(query) = server_handle);
        assert_eq!(query.0, "foo=bar");
    }

    #[async_test]
    async fn test_local_server_builder_early_shutdown() {
        let (mut uri, server_handle) = LocalServerBuilder::new().spawn().await.unwrap();
        uri.set_query(Some("foo=bar"));

        server_handle.shutdown_handle().shutdown();

        let http_client = reqwest::Client::new();
        http_client.get(uri.as_str()).send().await.unwrap_err();

        assert_let_timeout!(None = server_handle);
    }
}
