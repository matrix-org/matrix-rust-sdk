use matrix_sdk::{
    config::RequestConfig,
    matrix_auth::{MatrixSession, MatrixSessionTokens},
    media::{MediaFormat, MediaRequest, MediaThumbnailSettings, MediaThumbnailSize},
    test_utils::logged_in_client_with_server,
    Client, SessionMeta,
};
use matrix_sdk_test::async_test;
use ruma::{
    api::client::media::get_content_thumbnail::v3::Method,
    assign, device_id,
    events::room::{message::ImageMessageEventContent, ImageInfo, MediaSource},
    mxc_uri, uint, user_id,
};
use serde_json::json;
use wiremock::{
    matchers::{header, method, path, query_param},
    Mock, ResponseTemplate,
};

#[async_test]
async fn test_get_media_content_no_auth() {
    let (client, server) = logged_in_client_with_server().await;

    // The client will call this endpoint to get the list of unstable features.
    Mock::given(method("GET"))
        .and(path("/_matrix/client/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "versions": ["r0.6.1"],
        })))
        .named("versions")
        .expect(1)
        .mount(&server)
        .await;

    let media = client.media();

    let request = MediaRequest {
        source: MediaSource::Plain(mxc_uri!("mxc://localhost/textfile").to_owned()),
        format: MediaFormat::File,
    };

    // First time, without the cache.
    {
        let expected_content = "Hello, World!";
        let _mock_guard = Mock::given(method("GET"))
            .and(path("/_matrix/media/r0/download/localhost/textfile"))
            .respond_with(ResponseTemplate::new(200).set_body_string(expected_content))
            .named("get_file_no_cache")
            .expect(1)
            .mount_as_scoped(&server)
            .await;

        assert_eq!(
            media.get_media_content(&request, false).await.unwrap(),
            expected_content.as_bytes()
        );
    }

    // Second time, without the cache, error from the HTTP server.
    {
        let _mock_guard = Mock::given(method("GET"))
            .and(path("/_matrix/media/r0/download/localhost/textfile"))
            .respond_with(ResponseTemplate::new(500))
            .named("get_file_no_cache_error")
            .expect(1)
            .mount_as_scoped(&server)
            .await;

        assert!(media.get_media_content(&request, false).await.is_err());
    }

    let expected_content = "Hello, World (2)!";

    // Third time, with the cache.
    {
        let _mock_guard = Mock::given(method("GET"))
            .and(path("/_matrix/media/r0/download/localhost/textfile"))
            .respond_with(ResponseTemplate::new(200).set_body_string(expected_content))
            .named("get_file_with_cache")
            .expect(1)
            .mount_as_scoped(&server)
            .await;

        assert_eq!(
            media.get_media_content(&request, true).await.unwrap(),
            expected_content.as_bytes()
        );
    }

    // Third time, with the cache, the HTTP server isn't reached.
    {
        let _mock_guard = Mock::given(method("GET"))
            .and(path("/_matrix/media/r0/download/localhost/textfile"))
            .respond_with(ResponseTemplate::new(500))
            .named("get_file_with_cache_error")
            .expect(0)
            .mount_as_scoped(&server)
            .await;

        assert_eq!(
            client.media().get_media_content(&request, true).await.unwrap(),
            expected_content.as_bytes()
        );
    }
}

#[async_test]
async fn test_get_media_file_no_auth() {
    let (client, server) = logged_in_client_with_server().await;

    // The client will call this endpoint to get the list of unstable features.
    Mock::given(method("GET"))
        .and(path("/_matrix/client/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "versions": ["r0.6.1"],
        })))
        .named("versions")
        .expect(1)
        .mount(&server)
        .await;

    let event_content = ImageMessageEventContent::plain(
        "filename.jpg".into(),
        mxc_uri!("mxc://example.org/image").to_owned(),
    )
    .info(Box::new(assign!(ImageInfo::new(), {
        height: Some(uint!(398)),
        width: Some(uint!(394)),
        mimetype: Some("image/jpeg".into()),
        size: Some(uint!(31037)),
    })));

    // Get the file.
    Mock::given(method("GET"))
        .and(path("/_matrix/media/r0/download/example.org/image"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("binaryjpegdata", "image/jpeg"))
        .named("get_file")
        .expect(1)
        .mount(&server)
        .await;

    client.media().get_file(&event_content, false).await.unwrap();

    // Get a thumbnail, not animated.
    Mock::given(method("GET"))
        .and(path("/_matrix/media/r0/thumbnail/example.org/image"))
        .and(query_param("method", "scale"))
        .and(query_param("width", "100"))
        .and(query_param("height", "100"))
        .and(query_param("animated", "false"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("smallerbinaryjpegdata", "image/jpeg"),
        )
        .expect(1)
        .named("get_thumbnail_no_animated")
        .mount(&server)
        .await;

    client
        .media()
        .get_thumbnail(
            &event_content,
            MediaThumbnailSettings::new(Method::Scale, uint!(100), uint!(100)),
            true,
        )
        .await
        .unwrap();

    // Get a thumbnail, animated.
    Mock::given(method("GET"))
        .and(path("/_matrix/media/r0/thumbnail/example.org/image"))
        .and(query_param("method", "crop"))
        .and(query_param("width", "100"))
        .and(query_param("height", "100"))
        .and(query_param("animated", "true"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("smallerbinaryjpegdata", "image/jpeg"),
        )
        .expect(1)
        .named("get_thumbnail_animated_true")
        .mount(&server)
        .await;

    let settings = MediaThumbnailSettings {
        size: MediaThumbnailSize { method: Method::Crop, width: uint!(100), height: uint!(100) },
        animated: true,
    };
    client.media().get_thumbnail(&event_content, settings, true).await.unwrap();
}

#[async_test]
async fn test_get_media_file_with_auth_matrix_1_11() {
    // The server must advertise support for v1.11 for authenticated media support,
    // so we make the request instead of assuming.
    let server = wiremock::MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "versions": ["v1.7", "v1.8", "v1.9", "v1.10", "v1.11"],
        })))
        .named("versions")
        .expect(1)
        .mount(&server)
        .await;

    // Build client.
    let client = Client::builder()
        .homeserver_url(server.uri())
        .request_config(RequestConfig::new().disable_retry())
        .build()
        .await
        .unwrap();

    // Restore session.
    client
        .matrix_auth()
        .restore_session(MatrixSession {
            meta: SessionMeta {
                user_id: user_id!("@example:localhost").to_owned(),
                device_id: device_id!("DEVICEID").to_owned(),
            },
            tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
        })
        .await
        .unwrap();

    // Build event content.
    let event_content = ImageMessageEventContent::plain(
        "filename.jpg".into(),
        mxc_uri!("mxc://example.org/image").to_owned(),
    )
    .info(Box::new(assign!(ImageInfo::new(), {
        height: Some(uint!(398)),
        width: Some(uint!(394)),
        mimetype: Some("image/jpeg".into()),
        size: Some(uint!(31037)),
    })));

    // Get the full file.
    Mock::given(method("GET"))
        .and(path("/_matrix/client/v1/media/download/example.org/image"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("binaryjpegdata", "image/jpeg"))
        .named("get_file")
        .expect(1)
        .mount(&server)
        .await;

    client.media().get_file(&event_content, false).await.unwrap();

    // Get a thumbnail, not animated.
    Mock::given(method("GET"))
        .and(path("/_matrix/client/v1/media/thumbnail/example.org/image"))
        .and(query_param("method", "scale"))
        .and(query_param("width", "100"))
        .and(query_param("height", "100"))
        .and(query_param("animated", "false"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("smallerbinaryjpegdata", "image/jpeg"),
        )
        .expect(1)
        .named("get_thumbnail_no_animated")
        .mount(&server)
        .await;

    client
        .media()
        .get_thumbnail(
            &event_content,
            MediaThumbnailSettings::new(Method::Scale, uint!(100), uint!(100)),
            true,
        )
        .await
        .unwrap();

    // Get a thumbnail, animated.
    Mock::given(method("GET"))
        .and(path("/_matrix/client/v1/media/thumbnail/example.org/image"))
        .and(query_param("method", "crop"))
        .and(query_param("width", "100"))
        .and(query_param("height", "100"))
        .and(query_param("animated", "true"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("smallerbinaryjpegdata", "image/jpeg"),
        )
        .expect(1)
        .named("get_thumbnail_animated_true")
        .mount(&server)
        .await;

    let settings = MediaThumbnailSettings {
        size: MediaThumbnailSize { method: Method::Crop, width: uint!(100), height: uint!(100) },
        animated: true,
    };
    client.media().get_thumbnail(&event_content, settings, true).await.unwrap();
}

#[async_test]
async fn test_get_media_file_with_auth_matrix_stable_feature() {
    // The server must advertise support for the stable feature for authenticated
    // media support, so we make the request instead of assuming.
    let server = wiremock::MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "versions": ["v1.7", "v1.8", "v1.9", "v1.10"],
            "unstable_features": {
                "org.matrix.msc3916.stable": true,
            },
        })))
        .named("versions")
        .expect(1)
        .mount(&server)
        .await;

    // Build client.
    let client = Client::builder()
        .homeserver_url(server.uri())
        .request_config(RequestConfig::new().disable_retry())
        .build()
        .await
        .unwrap();

    // Restore session.
    client
        .matrix_auth()
        .restore_session(MatrixSession {
            meta: SessionMeta {
                user_id: user_id!("@example:localhost").to_owned(),
                device_id: device_id!("DEVICEID").to_owned(),
            },
            tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
        })
        .await
        .unwrap();

    // Build event content.
    let event_content = ImageMessageEventContent::plain(
        "filename.jpg".into(),
        mxc_uri!("mxc://example.org/image").to_owned(),
    )
    .info(Box::new(assign!(ImageInfo::new(), {
        height: Some(uint!(398)),
        width: Some(uint!(394)),
        mimetype: Some("image/jpeg".into()),
        size: Some(uint!(31037)),
    })));

    // Get the full file.
    Mock::given(method("GET"))
        .and(path("/_matrix/client/v1/media/download/example.org/image"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("binaryjpegdata", "image/jpeg"))
        .named("get_file")
        .expect(1)
        .mount(&server)
        .await;

    client.media().get_file(&event_content, false).await.unwrap();

    // Get a thumbnail, not animated.
    Mock::given(method("GET"))
        .and(path("/_matrix/client/v1/media/thumbnail/example.org/image"))
        .and(query_param("method", "scale"))
        .and(query_param("width", "100"))
        .and(query_param("height", "100"))
        .and(query_param("animated", "false"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("smallerbinaryjpegdata", "image/jpeg"),
        )
        .expect(1)
        .named("get_thumbnail_no_animated")
        .mount(&server)
        .await;

    client
        .media()
        .get_thumbnail(
            &event_content,
            MediaThumbnailSettings::new(Method::Scale, uint!(100), uint!(100)),
            true,
        )
        .await
        .unwrap();

    // Get a thumbnail, animated.
    Mock::given(method("GET"))
        .and(path("/_matrix/client/v1/media/thumbnail/example.org/image"))
        .and(query_param("method", "crop"))
        .and(query_param("width", "100"))
        .and(query_param("height", "100"))
        .and(query_param("animated", "true"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("smallerbinaryjpegdata", "image/jpeg"),
        )
        .expect(1)
        .named("get_thumbnail_animated_true")
        .mount(&server)
        .await;

    let settings = MediaThumbnailSettings {
        size: MediaThumbnailSize { method: Method::Crop, width: uint!(100), height: uint!(100) },
        animated: true,
    };
    client.media().get_thumbnail(&event_content, settings, true).await.unwrap();
}
