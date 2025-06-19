use matrix_sdk::{
    Client,
    config::RequestConfig,
    media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings},
    store::RoomLoadSettings,
    test_utils::{client::mock_matrix_session, logged_in_client_with_server},
};
use matrix_sdk_test::async_test;
use ruma::{
    api::client::media::get_content_thumbnail::v3::Method,
    assign,
    events::room::{ImageInfo, MediaSource, message::ImageMessageEventContent},
    mxc_uri, owned_mxc_uri, uint,
};
use serde_json::json;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{header, method, path, query_param},
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

    let request = MediaRequestParameters {
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
            MediaThumbnailSettings::with_method(Method::Scale, uint!(100), uint!(100)),
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
        method: Method::Crop,
        width: uint!(100),
        height: uint!(100),
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
        .restore_session(mock_matrix_session(), RoomLoadSettings::default())
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
            MediaThumbnailSettings::with_method(Method::Scale, uint!(100), uint!(100)),
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
        method: Method::Crop,
        width: uint!(100),
        height: uint!(100),
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
        .restore_session(mock_matrix_session(), RoomLoadSettings::default())
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
            MediaThumbnailSettings::with_method(Method::Scale, uint!(100), uint!(100)),
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
        method: Method::Crop,
        width: uint!(100),
        height: uint!(100),
        animated: true,
    };
    client.media().get_thumbnail(&event_content, settings, true).await.unwrap();
}

#[async_test]
async fn test_async_media_upload() {
    let (client, server) = logged_in_client_with_server().await;

    client.reset_server_capabilities().await.unwrap();

    // Declare Matrix version v1.7.
    Mock::given(method("GET"))
        .and(path("/_matrix/client/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "versions": [
                "v1.7"
            ],
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/media/v1/create"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
          "content_uri": "mxc://example.com/AQwafuaFswefuhsfAFAgsw"
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path("/_matrix/media/v3/upload/example.com/AQwafuaFswefuhsfAFAgsw"))
        .and(header("authorization", "Bearer 1234"))
        .and(header("content-type", "image/jpeg"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let mxc_uri = client.media().create_content_uri().await.unwrap();

    assert_eq!(mxc_uri.uri, owned_mxc_uri!("mxc://example.com/AQwafuaFswefuhsfAFAgsw"));

    client
        .media()
        .upload_preallocated(mxc_uri, &mime::IMAGE_JPEG, b"hello world".to_vec())
        .await
        .unwrap();
}
