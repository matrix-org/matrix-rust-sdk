use std::{env, io, process::exit};
use url::Url;

use matrix_sdk::{
    self, events::AnyToDeviceEvent, identifiers::UserId, Client, ClientConfig, Sas, SyncSettings,
};

async fn wait_for_confirmation(client: Client, sas: Sas) {
    println!("Does the emoji match: {:?}", sas.emoji());

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .expect("error: unable to read user input");

    match input.trim().to_lowercase().as_ref() {
        "yes" | "true" | "ok" => {
            sas.confirm().await.unwrap();

            if sas.is_done() {
                print_result(&sas);
                print_devices(sas.other_device().user_id(), &client).await;
            }
        }
        _ => sas.cancel().await.unwrap(),
    }
}

fn print_result(sas: &Sas) {
    let device = sas.other_device();

    println!(
        "Successfully verified device {} {} {:?}",
        device.user_id(),
        device.device_id(),
        device.local_trust_state()
    );
}

async fn print_devices(user_id: &UserId, client: &Client) {
    println!("Devices of user {}", user_id);

    for device in client.get_user_devices(user_id).await.unwrap().devices() {
        println!(
            "   {:<10} {:<30} {:<}",
            device.device_id(),
            device.display_name().as_deref().unwrap_or_default(),
            device.is_trusted()
        );
    }
}

async fn login(
    homeserver_url: String,
    username: &str,
    password: &str,
) -> Result<(), matrix_sdk::Error> {
    let client_config = ClientConfig::new()
        .disable_ssl_verification()
        .proxy("http://localhost:8080")
        .unwrap();
    let homeserver_url = Url::parse(&homeserver_url).expect("Couldn't parse the homeserver URL");
    let client = Client::new_with_config(homeserver_url, client_config).unwrap();

    client
        .login(username, password, None, Some("rust-sdk"))
        .await?;

    let client_ref = &client;

    client
        .sync_forever(SyncSettings::new(), |response| async move {
            let client = &client_ref;

            for event in &response.to_device.events {
                let e = event
                    .deserialize()
                    .expect("Can't deserialize to-device event");

                match e {
                    AnyToDeviceEvent::KeyVerificationStart(e) => {
                        let sas = client
                            .get_verification(&e.content.transaction_id)
                            .await
                            .expect("Sas object wasn't created");
                        println!(
                            "Starting verification with {} {}",
                            &sas.other_device().user_id(),
                            &sas.other_device().device_id()
                        );
                        print_devices(&e.sender, &client).await;
                        sas.accept().await.unwrap();
                    }

                    AnyToDeviceEvent::KeyVerificationKey(e) => {
                        let sas = client
                            .get_verification(&e.content.transaction_id)
                            .await
                            .expect("Sas object wasn't created");

                        tokio::spawn(wait_for_confirmation((*client).clone(), sas));
                    }

                    AnyToDeviceEvent::KeyVerificationMac(e) => {
                        let sas = client
                            .get_verification(&e.content.transaction_id)
                            .await
                            .expect("Sas object wasn't created");

                        if sas.is_done() {
                            print_result(&sas);
                            print_devices(&e.sender, &client).await;
                        }
                    }

                    _ => (),
                }
            }
        })
        .await;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), matrix_sdk::Error> {
    tracing_subscriber::fmt::init();

    let (homeserver_url, username, password) =
        match (env::args().nth(1), env::args().nth(2), env::args().nth(3)) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => {
                eprintln!(
                    "Usage: {} <homeserver_url> <username> <password>",
                    env::args().next().unwrap()
                );
                exit(1)
            }
        };

    login(homeserver_url, &username, &password).await
}
