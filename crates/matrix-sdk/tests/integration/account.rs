use assert_matches2::assert_matches;
use matrix_sdk::test_utils::mocks::MatrixMockServer;
use matrix_sdk_test::async_test;
use ruma::{
    api::{
        MatrixVersion,
        client::profile::{ProfileFieldName, ProfileFieldValue, TimeZone},
    },
    mxc_uri,
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
