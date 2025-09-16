use matrix_sdk::{
    media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings},
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::async_test;
use ruma::{
    api::client::media::get_content_thumbnail::v3::Method,
    assign,
    events::room::{ImageInfo, MediaSource, message::ImageMessageEventContent},
    mxc_uri, owned_mxc_uri, uint,
};

#[async_test]
async fn test_get_media_content_no_auth() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().no_server_versions().build().await;
    let expected_content = b"Hello, World!";

    server
        .mock_versions()
        .ok_custom(&["v1.1"], &Default::default())
        .named("versions")
        .expect(1)
        .mount()
        .await;

    let media = client.media();

    let request = MediaRequestParameters {
        source: MediaSource::Plain(mxc_uri!("mxc://localhost/textfile").to_owned()),
        format: MediaFormat::File,
    };

    // First time, without the cache.
    {
        let _mock_guard = server
            .mock_media_download()
            .ok_plain_text()
            .named("get_file_no_cache")
            .expect(1)
            .mount_as_scoped()
            .await;

        assert_eq!(media.get_media_content(&request, false).await.unwrap(), expected_content);
    }

    // Second time, without the cache, error from the HTTP server.
    {
        let _mock_guard = server
            .mock_media_download()
            .error500()
            .named("get_file_no_cache_error")
            .expect(1)
            .mount_as_scoped()
            .await;

        assert!(media.get_media_content(&request, false).await.is_err());
    }

    // Third time, with the cache.
    {
        let _mock_guard = server
            .mock_media_download()
            .ok_plain_text()
            .named("get_file_with_cache")
            .expect(1)
            .mount_as_scoped()
            .await;

        assert_eq!(media.get_media_content(&request, true).await.unwrap(), expected_content);
    }

    // Third time, with the cache, the HTTP server isn't reached.
    {
        let _mock_guard = server
            .mock_media_download()
            .error500()
            .named("get_file_with_cache_error")
            .expect(0)
            .mount_as_scoped()
            .await;

        assert_eq!(
            client.media().get_media_content(&request, true).await.unwrap(),
            expected_content
        );
    }
}

#[async_test]
async fn test_get_media_file_no_auth() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().no_server_versions().build().await;

    server
        .mock_versions()
        .ok_custom(&["v1.1"], &Default::default())
        .named("versions")
        .expect(1)
        .mount()
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
    server.mock_media_download().ok_image().named("get_file").expect(1).mount().await;

    client.media().get_file(&event_content, false).await.unwrap();

    // Get a thumbnail, not animated.
    server
        .mock_media_thumbnail(Method::Scale, 100, 100, false)
        .ok()
        .expect(1)
        .named("get_thumbnail_no_animated")
        .mount()
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
    server
        .mock_media_thumbnail(Method::Crop, 100, 100, true)
        .ok()
        .expect(1)
        .named("get_thumbnail_animated_true")
        .mount()
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
    // The server must advertise support for v1.11 or newer for authenticated media
    // support, so we make the request instead of assuming.
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().no_server_versions().build().await;

    server
        .mock_versions()
        .ok_custom(&["v1.11"], &Default::default())
        .expect(1)
        .named("versions")
        .mount()
        .await;

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
    server.mock_authed_media_download().ok_image().named("get_file").expect(1).mount().await;

    client.media().get_file(&event_content, false).await.unwrap();

    // Get a thumbnail, not animated.
    server
        .mock_authed_media_thumbnail(Method::Scale, 100, 100, false)
        .ok()
        .expect(1)
        .named("get_thumbnail_no_animated")
        .mount()
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
    server
        .mock_authed_media_thumbnail(Method::Crop, 100, 100, true)
        .ok()
        .expect(1)
        .named("get_thumbnail_animated_true")
        .mount()
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
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().no_server_versions().build().await;

    server
        .mock_versions()
        .ok_custom(
            &["v1.7", "v1.8", "v1.9", "v1.10"],
            &[("org.matrix.msc3916.stable", true)].into(),
        )
        .named("versions")
        .expect(1)
        .mount()
        .await;

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
    server.mock_authed_media_download().ok_image().named("get_file").expect(1).mount().await;

    client.media().get_file(&event_content, false).await.unwrap();

    // Get a thumbnail, not animated.
    server
        .mock_authed_media_thumbnail(Method::Scale, 100, 100, false)
        .ok()
        .expect(1)
        .named("get_thumbnail_no_animated")
        .mount()
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
    server
        .mock_authed_media_thumbnail(Method::Crop, 100, 100, true)
        .ok()
        .expect(1)
        .named("get_thumbnail_animated_true")
        .mount()
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
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().no_server_versions().build().await;

    // Declare Matrix version v1.7.
    server.mock_versions().ok_custom(&["v1.7"], &Default::default()).expect(1).mount().await;

    server.mock_media_allocate().ok().expect(1).mount().await;

    server
        .mock_media_allocated_upload("example.com", "AQwafuaFswefuhsfAFAgsw")
        .ok()
        .expect(1)
        .mount()
        .await;

    let mxc_uri = client.media().create_content_uri().await.unwrap();

    assert_eq!(mxc_uri.uri, owned_mxc_uri!("mxc://example.com/AQwafuaFswefuhsfAFAgsw"));

    client
        .media()
        .upload_preallocated(mxc_uri, &mime::IMAGE_JPEG, b"hello world".to_vec())
        .await
        .unwrap();
}
