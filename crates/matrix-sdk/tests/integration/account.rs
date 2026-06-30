use assert_matches2::assert_matches;
use matrix_sdk::test_utils::mocks::MatrixMockServer;
use matrix_sdk_test::async_test;
use ruma::{
    api::{
        MatrixVersion,
        client::profile::{AvatarUrl, DisplayName, TimeZone},
    },
    mxc_uri,
    profile::{ProfileFieldName, ProfileFieldValue},
};
use serde_json::json;
use wiremock::{
    Mock, Request, ResponseTemplate,
    matchers::{method, path},
};

use crate::logged_in_client_with_server;

#[async_test]
async fn test_account_deactivation() {
    #[derive(serde::Deserialize)]
    struct Parameters {
        pub id_server: Option<String>,
        pub erase: Option<bool>,
    }

    let (client, server) = logged_in_client_with_server().await;

    {
        let _scope = Mock::given(method("POST"))
            .and(path("/_matrix/client/r0/account/deactivate"))
            .respond_with(|req: &Request| {
                let params: Parameters = req.body_json().unwrap();
                assert_eq!(params.id_server, Some("FirstIdentityServer".to_owned()));
                assert_eq!(params.erase, None);

                ResponseTemplate::new(200).set_body_json(json!({
                    "id_server_unbind_result": "success"
                }))
            })
            .expect(1)
            .mount_as_scoped(&server)
            .await;

        assert!(
            client.account().deactivate(Some("FirstIdentityServer"), None, false).await.is_ok()
        );
    }

    {
        let _scope = Mock::given(method("POST"))
            .and(path("/_matrix/client/r0/account/deactivate"))
            .respond_with(|req: &Request| {
                let params: Parameters = req.body_json().unwrap();
                assert_eq!(params.id_server, None);
                assert_eq!(params.erase, Some(true));

                ResponseTemplate::new(200).set_body_json(json!({
                    "id_server_unbind_result": "success"
                }))
            })
            .expect(1)
            .mount_as_scoped(&server)
            .await;

        assert!(client.account().deactivate(None, None, true).await.is_ok());
    }
}

#[async_test]
async fn test_fetch_profile_field() {
    let tz = "Africa/Bujumbura";
    let display_name = "Alice";

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().server_versions(vec![MatrixVersion::V1_16]).build().await;
    let user_id = client.user_id().unwrap();

    server
        .mock_get_profile_field(user_id, ProfileFieldName::TimeZone)
        .ok_with_value(Some(tz.into()))
        .expect(2)
        .named("get m.tz profile field")
        .mount()
        .await;
    server
        .mock_get_profile_field(user_id, ProfileFieldName::DisplayName)
        .ok_with_value(Some(display_name.into()))
        .mock_once()
        .named("get displayname profile field")
        .mount()
        .await;
    server
        .mock_get_profile_field(user_id, ProfileFieldName::AvatarUrl)
        .ok_with_value(None)
        .mock_once()
        .named("get avatar_url profile field")
        .mount()
        .await;

    let account = client.account();

    let res_avatar_url = account.get_avatar_url().await.unwrap();
    assert_eq!(res_avatar_url, None);
    let res_display_name = account.get_display_name().await.unwrap();
    assert_eq!(res_display_name.as_deref(), Some(display_name));
    let res_value = account
        .fetch_profile_field_of(user_id.to_owned(), ProfileFieldName::TimeZone)
        .await
        .unwrap();
    assert_matches!(res_value, Some(ProfileFieldValue::TimeZone(res_tz)));
    assert_eq!(res_tz, tz);
    let res_tz =
        account.fetch_profile_field_of_static::<TimeZone>(user_id.to_owned()).await.unwrap();
    assert_eq!(res_tz.as_deref(), Some(tz));
}

#[async_test]
async fn test_set_profile_field() {
    let tz = "Africa/Bujumbura";
    let display_name = "Alice";
    let avatar_url = mxc_uri!("mxc://localhost/1mA63");

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().server_versions(vec![MatrixVersion::V1_16]).build().await;
    let user_id = client.user_id().unwrap();

    server
        .mock_set_profile_field(user_id, ProfileFieldName::TimeZone)
        .ok()
        .mock_once()
        .named("set m.tz profile field")
        .mount()
        .await;
    server
        .mock_set_profile_field(user_id, ProfileFieldName::DisplayName)
        .ok()
        .mock_once()
        .named("set displayname profile field")
        .mount()
        .await;
    server
        .mock_set_profile_field(user_id, ProfileFieldName::AvatarUrl)
        .ok()
        .mock_once()
        .named("set avatar_url profile field")
        .mount()
        .await;

    let account = client.account();

    account.set_avatar_url(Some(avatar_url)).await.unwrap();
    account.set_display_name(Some(display_name)).await.unwrap();
    account.set_profile_field(ProfileFieldValue::TimeZone(tz.to_owned())).await.unwrap();
}

#[async_test]
async fn test_delete_profile_field() {
    let server = MatrixMockServer::new().await;

    // Test with server that does NOT support deleting custom fields.
    {
        let client =
            server.client_builder().server_versions(vec![MatrixVersion::V1_15]).build().await;
        let user_id = client.user_id().unwrap();

        let _guard = server
            .mock_set_profile_field(user_id, ProfileFieldName::DisplayName)
            .ok()
            .mock_once()
            .named("set displayname profile field")
            .mount_as_scoped()
            .await;
        let _guard = server
            .mock_set_profile_field(user_id, ProfileFieldName::AvatarUrl)
            .ok()
            .mock_once()
            .named("set avatar_url profile field")
            .mount_as_scoped()
            .await;

        let account = client.account();

        account.set_avatar_url(None).await.unwrap();
        account.set_display_name(None).await.unwrap();
    }

    // Test with server that supports deleting custom fields.
    {
        let client =
            server.client_builder().server_versions(vec![MatrixVersion::V1_16]).build().await;
        let user_id = client.user_id().unwrap();

        let _guard = server
            .mock_delete_profile_field(user_id, ProfileFieldName::AvatarUrl)
            .ok()
            .mock_once()
            .named("delete m.tz profile field")
            .mount_as_scoped()
            .await;
        let _guard = server
            .mock_delete_profile_field(user_id, ProfileFieldName::DisplayName)
            .ok()
            .mock_once()
            .named("delete m.tz profile field")
            .mount_as_scoped()
            .await;
        let _guard = server
            .mock_delete_profile_field(user_id, ProfileFieldName::TimeZone)
            .ok()
            .mock_once()
            .named("delete m.tz profile field")
            .mount_as_scoped()
            .await;

        let account = client.account();

        account.set_avatar_url(None).await.unwrap();
        account.set_display_name(None).await.unwrap();
        account.delete_profile_field(ProfileFieldName::TimeZone).await.unwrap();
    }
}

#[async_test]
async fn test_fetch_user_profile() {
    let tz = "Africa/Bujumbura";
    let display_name = "Alice";

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();

    server
        .mock_get_profile(user_id)
        .ok_with_fields(vec![
            ProfileFieldValue::TimeZone(tz.to_owned()),
            ProfileFieldValue::DisplayName(display_name.to_owned()),
        ])
        .mock_once()
        .named("get profile")
        .mount()
        .await;

    let profile = client.account().fetch_user_profile().await.unwrap();

    assert_eq!(profile.get_static::<TimeZone>().unwrap().as_deref(), Some(tz));
    assert_eq!(profile.get_static::<DisplayName>().unwrap().as_deref(), Some(display_name));
    assert_eq!(profile.get_static::<AvatarUrl>().unwrap(), None);
}

#[async_test]
async fn test_fetch_removed_avatar_url() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().server_versions(vec![MatrixVersion::V1_16]).build().await;
    let user_id = client.user_id().unwrap();

    server
        .mock_get_profile_field(user_id, ProfileFieldName::AvatarUrl)
        .respond_with(ResponseTemplate::new(200).set_body_json(
            // This is what Synapse returns after calling Account::set_avatar_url(None).
            json!({"avatar_url":null}),
        ))
        .mock_once()
        .named("get avatar_url")
        .mount()
        .await;

    let account = client.account();

    let res_avatar_url = account.get_avatar_url().await.unwrap();
    assert_eq!(res_avatar_url, None);
}

#[async_test]
async fn test_get_cached_avatar_url() {
    let avatar_url = mxc_uri!("mxc://localhost/1mA63");

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();

    let account = client.account();

    // The cache is empty.
    let res_avatar_url = account.get_cached_avatar_url().await.unwrap();
    assert_eq!(res_avatar_url, None);

    // Fetch it from the homeserver, it should fill the cache.
    {
        let _guard = server
            .mock_get_profile_field(user_id, ProfileFieldName::AvatarUrl)
            .ok_with_value(Some(avatar_url.as_str().into()))
            .mock_once()
            .named("get avatar_url profile field with value")
            .mount_as_scoped()
            .await;

        let res_avatar_url = account.get_avatar_url().await.unwrap();
        assert_eq!(res_avatar_url.as_deref(), Some(avatar_url));
    }

    // The cache was filled.
    let res_avatar_url = account.get_cached_avatar_url().await.unwrap();
    assert_eq!(res_avatar_url.as_deref(), Some(avatar_url));

    // Fetch it again from the homeserver, a missing value should empty the cache.
    {
        let _guard = server
            .mock_get_profile_field(user_id, ProfileFieldName::AvatarUrl)
            .ok_with_value(None)
            .mock_once()
            .named("get avatar_url profile field without value")
            .mount_as_scoped()
            .await;

        let res_avatar_url = account.get_avatar_url().await.unwrap();
        assert_eq!(res_avatar_url, None);
    }

    // The cache was emptied.
    let res_avatar_url = account.get_cached_avatar_url().await.unwrap();
    assert_eq!(res_avatar_url, None);
}

#[cfg(feature = "unstable-msc4426")]
#[async_test]
async fn test_set_status() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().server_versions(vec![MatrixVersion::V1_16]).build().await;
    let user_id = client.user_id().unwrap();

    server
        .mock_set_profile_field(user_id, ProfileFieldName::Status)
        .ok()
        .mock_once()
        .named("set org.matrix.msc4426.status profile field")
        .mount()
        .await;

    let account = client.account();
    account.set_status("🌴".to_owned(), "Away".to_owned()).await.unwrap();
}

#[cfg(feature = "unstable-msc4426")]
#[async_test]
async fn test_clear_status() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().server_versions(vec![MatrixVersion::V1_16]).build().await;
    let user_id = client.user_id().unwrap();

    server
        .mock_delete_profile_field(user_id, ProfileFieldName::Status)
        .ok()
        .mock_once()
        .named("delete org.matrix.msc4426.status profile field")
        .mount()
        .await;

    let account = client.account();
    account.clear_status().await.unwrap();
}

#[cfg(feature = "unstable-msc4426")]
#[async_test]
async fn test_set_call() {
    use ruma::{SecondsSinceUnixEpoch, uint};

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().server_versions(vec![MatrixVersion::V1_16]).build().await;
    let user_id = client.user_id().unwrap();

    // Two PUTs expected: one with a join ts, one without.
    server
        .mock_set_profile_field(user_id, ProfileFieldName::Call)
        .ok()
        .expect(2)
        .named("set org.matrix.msc4426.call profile field")
        .mount()
        .await;

    let account = client.account();
    account.set_call(Some(SecondsSinceUnixEpoch(uint!(1_770_140_640)))).await.unwrap();
    account.set_call(None).await.unwrap();
}

#[cfg(feature = "unstable-msc4426")]
#[async_test]
async fn test_clear_call() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().server_versions(vec![MatrixVersion::V1_16]).build().await;
    let user_id = client.user_id().unwrap();

    server
        .mock_delete_profile_field(user_id, ProfileFieldName::Call)
        .ok()
        .mock_once()
        .named("delete org.matrix.msc4426.call profile field")
        .mount()
        .await;

    let account = client.account();
    account.clear_call().await.unwrap();
}

#[cfg(feature = "unstable-msc4426")]
#[async_test]
async fn test_fetch_user_profile_with_status() {
    use ruma::{
        SecondsSinceUnixEpoch,
        api::client::profile::{Call, Status},
        profile::{CallProfileField, StatusProfileField},
        uint,
    };

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();

    let mut call_field = CallProfileField::new();
    call_field.call_joined_ts = Some(SecondsSinceUnixEpoch(uint!(1_770_140_640)));

    server
        .mock_get_profile(user_id)
        .ok_with_fields(vec![
            ProfileFieldValue::DisplayName("Alice".to_owned()),
            ProfileFieldValue::AvatarUrl(mxc_uri!("mxc://localhost/abc").to_owned()),
            ProfileFieldValue::Status(StatusProfileField::new("Away".to_owned(), "🌴".to_owned())),
            ProfileFieldValue::Call(call_field),
        ])
        .mock_once()
        .named("get profile with status and call")
        .mount()
        .await;

    let profile = client.account().fetch_user_profile().await.unwrap();

    assert_eq!(profile.get_static::<DisplayName>().unwrap().as_deref(), Some("Alice"));
    let status = profile.get_static::<Status>().unwrap().unwrap();
    assert_eq!(status.emoji, "🌴");
    assert_eq!(status.text, "Away");
    let call = profile.get_static::<Call>().unwrap().unwrap();
    assert_eq!(call.call_joined_ts, Some(SecondsSinceUnixEpoch(uint!(1_770_140_640))));
}

#[cfg(feature = "unstable-msc4426")]
#[async_test]
async fn test_fetch_user_profile_call_without_ts() {
    use ruma::{
        api::client::profile::{Call, Status},
        profile::CallProfileField,
    };

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();

    server
        .mock_get_profile(user_id)
        .ok_with_fields(vec![
            ProfileFieldValue::DisplayName("Bob".to_owned()),
            ProfileFieldValue::Call(CallProfileField::new()),
        ])
        .mock_once()
        .named("get profile with call but no ts")
        .mount()
        .await;

    let profile = client.account().fetch_user_profile().await.unwrap();

    assert_eq!(profile.get_static::<Status>().unwrap(), None);
    let call = profile.get_static::<Call>().unwrap().unwrap();
    assert_eq!(call.call_joined_ts, None);
}
