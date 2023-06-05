use async_stream::stream;
use futures_core::stream::Stream;
use futures_util::{pin_mut, StreamExt};
use matrix_sdk::{sliding_sync::ExtensionsSetter, Client, SlidingSync};
use tracing::warn;

pub struct NotificationApi {
    sliding_sync: SlidingSync,
}

impl NotificationApi {
    /// Creates a new instance of a `NotificationApi`.
    ///
    /// The passed `id` is that of the underlying Sliding Sync; make sure to not reuse a name used
    /// by another Sliding Sync instance, at the risk of causing problems.
    pub async fn new(id: impl Into<String>, client: Client) -> Result<Self, Error> {
        let sliding_sync = client
            .sliding_sync(id)
            .map_err(Error::SlidingSyncError)?
            .enable_caching()?
            .with_extensions(ExtensionsSetter {
                to_device: true,
                e2ee: true,
                account_data: false,
                receipts: false,
                typing: false,
            })
            .build()
            .await?;

        Ok(Self { sliding_sync })
    }

    /// Start synchronization of notifications.
    ///
    /// This doesn't return any meaningful result, but this can be used to count successful
    /// iterations.
    pub fn run(&self) -> impl Stream<Item = Result<(), Error>> + '_ {
        stream!({
            let sync = self.sliding_sync.sync();

            pin_mut!(sync);

            loop {
                match sync.next().await {
                    Some(Ok(update_summary)) => {
                        // This API is only concerned with the e2ee and to-device extensions.
                        // Warn if anything weird has been received from the proxy.
                        if !update_summary.lists.is_empty() {
                            warn!(?update_summary.lists, "unexpected non-empty list of lists in notification API");
                        }
                        if !update_summary.rooms.is_empty() {
                            warn!(?update_summary.rooms, "unexpected non-empty list of rooms in notification API");
                        }

                        // Cool cool, let's do it again.
                        yield Ok(());

                        continue;
                    }

                    Some(Err(err)) => {
                        yield Err(err.into());

                        // TODO do better when running into an error?

                        break;
                    }

                    None => {
                        break;
                    }
                }
            }
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Something wrong happened in sliding sync")]
    SlidingSyncError(#[from] matrix_sdk::Error),
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use futures_util::{pin_mut, StreamExt as _};
    use matrix_sdk::{config::RequestConfig, Client, Session};
    use matrix_sdk_test::async_test;
    use ruma::{api::MatrixVersion, device_id, user_id};
    use serde_json::json;
    use wiremock::{http::Method, Match, Mock, MockServer, Request, ResponseTemplate};

    use crate::notifications::{Error, NotificationApi};

    // TODO copy-pasto from Ivan's PR: commonize!
    async fn new_client() -> (Client, MockServer) {
        let session = Session {
            access_token: "1234".to_owned(),
            refresh_token: None,
            user_id: user_id!("@example:localhost").to_owned(),
            device_id: device_id!("DEVICEID").to_owned(),
        };

        let server = MockServer::start().await;
        let client = Client::builder()
            .homeserver_url(server.uri())
            .server_versions([MatrixVersion::V1_0])
            .request_config(RequestConfig::new().disable_retry())
            .build()
            .await
            .unwrap();
        client.restore_session(session).await.unwrap();

        (client, server)
    }

    #[derive(Copy, Clone)]
    struct SlidingSyncMatcher;

    impl Match for SlidingSyncMatcher {
        fn matches(&self, request: &Request) -> bool {
            request.url.path() == "/_matrix/client/unstable/org.matrix.msc3575/sync"
                && request.method == Method::Post
        }
    }

    // TODO end of copy-pasto

    #[async_test]
    async fn test_smoke_test_notification_api() -> Result<(), Error> {
        let (client, server) = new_client().await;

        let _mock_guard = Mock::given(SlidingSyncMatcher)
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "pos": "0",
            })))
            .mount_as_scoped(&server)
            .await;

        let notification_api = NotificationApi::new("notifs".to_owned(), client).await?;

        let notification_stream = notification_api.run();

        pin_mut!(notification_stream);

        // In the default setting, there's always a notification.
        let notification = notification_stream.next().await;
        assert_matches!(notification, Some(Ok(())));

        let notification = notification_stream.next().await;
        assert_matches!(notification, Some(Ok(())));

        let notification = notification_stream.next().await;
        assert_matches!(notification, Some(Ok(())));

        Ok(())
    }

    // TODO add more tests, with responses that contain to-device or e2ee events.
}
