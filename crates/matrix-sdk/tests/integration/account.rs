use assert_matches::assert_matches;
use matrix_sdk_test::async_test;
use serde_json::json;
use wiremock::{
    matchers::{method, path},
    Mock, Request, ResponseTemplate,
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

        assert!(client
            .account()
            .deactivate(Some("FirstIdentityServer"), None, false)
            .await
            .is_ok());
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
