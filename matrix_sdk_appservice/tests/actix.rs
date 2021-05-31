#[cfg(feature = "actix")]
mod actix {
    use std::env;

    use actix_web::{test, App};
    use matrix_sdk_appservice::*;

    async fn appservice() -> Appservice {
        env::set_var("RUST_LOG", "mockito=debug,matrix_sdk=debug,ruma=debug,actix_web=debug");
        let _ = tracing_subscriber::fmt::try_init();

        Appservice::new(
            mockito::server_url().as_ref(),
            "test.local",
            AppserviceRegistration::try_from_yaml_str(include_str!("./registration.yaml")).unwrap(),
        )
        .await
        .unwrap()
    }

    #[actix_rt::test]
    async fn test_transactions() {
        let appservice = appservice().await;
        let app = test::init_service(App::new().service(appservice.actix_service())).await;

        let transactions = r#"{
            "events": [
                {
                    "content": {},
                    "type": "m.dummy"
                }
            ]
        }"#;

        let transactions: serde_json::Value = serde_json::from_str(transactions).unwrap();

        let req = test::TestRequest::put()
            .uri("/_matrix/app/v1/transactions/1?access_token=hs_token")
            .set_json(&transactions)
            .to_request();

        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), 200);
    }

    #[actix_rt::test]
    async fn test_users() {
        let appservice = appservice().await;
        let app = test::init_service(App::new().service(appservice.actix_service())).await;

        let req = test::TestRequest::get()
            .uri("/_matrix/app/v1/users/%40_botty_1%3Adev.famedly.local?access_token=hs_token")
            .to_request();

        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), 200);
    }

    #[actix_rt::test]
    async fn test_invalid_access_token() {
        let appservice = appservice().await;
        let app = test::init_service(App::new().service(appservice.actix_service())).await;

        let transactions = r#"{
            "events": [
                {
                    "content": {},
                    "type": "m.dummy"
                }
            ]
        }"#;

        let transactions: serde_json::Value = serde_json::from_str(transactions).unwrap();

        let req = test::TestRequest::put()
            .uri("/_matrix/app/v1/transactions/1?access_token=invalid_token")
            .set_json(&transactions)
            .to_request();

        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), 401);
    }

    #[actix_rt::test]
    async fn test_no_access_token() {
        let appservice = appservice().await;
        let app = test::init_service(App::new().service(appservice.actix_service())).await;

        let transactions = r#"{
            "events": [
                {
                    "content": {},
                    "type": "m.dummy"
                }
            ]
        }"#;

        let transactions: serde_json::Value = serde_json::from_str(transactions).unwrap();

        let req = test::TestRequest::put()
            .uri("/_matrix/app/v1/transactions/1")
            .set_json(&transactions)
            .to_request();

        let resp = test::call_service(&app, req).await;

        // TODO: this should actually return a 401 but is 500 because something in the
        // extractor fails
        assert_eq!(resp.status(), 500);
    }
}
