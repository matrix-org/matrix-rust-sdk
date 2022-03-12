use std::{
    env, io,
    process::exit,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use matrix_sdk::{
    self,
    config::SyncSettings,
    encryption::verification::{SasVerification, Verification},
    ruma::{
        events::{
            room::message::MessageType, AnySyncMessageEvent, AnySyncRoomEvent, AnyToDeviceEvent,
        },
        UserId,
    },
    Client, LoopCtrl,
};
use url::Url;

async fn wait_for_confirmation(client: Client, sas: SasVerification) {
    println!("Does the emoji match: {:?}", sas.emoji());

    let mut input = String::new();
    io::stdin().read_line(&mut input).expect("error: unable to read user input");

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

fn print_result(sas: &SasVerification) {
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

    for device in client.encryption().get_user_devices(user_id).await.unwrap().devices() {
        println!(
            "   {:<10} {:<30} {:<}",
            device.device_id(),
            device.display_name().unwrap_or("-"),
            device.verified()
        );
    }
}

async fn login(
    homeserver_url: String,
    username: &str,
    password: &str,
) -> Result<(), matrix_sdk::Error> {
    let homeserver_url = Url::parse(&homeserver_url).expect("Couldn't parse the homeserver URL");
    let client = Client::new(homeserver_url).await.unwrap();

    client.login(username, password, None, Some("rust-sdk")).await?;

    let client_ref = &client;
    let initial_sync = Arc::new(AtomicBool::from(true));
    let initial_ref = &initial_sync;

    client
        .sync_with_callback(SyncSettings::new(), |response| async move {
            let client = &client_ref;
            let initial = &initial_ref;

            for event in response.to_device.events.iter().filter_map(|e| e.deserialize().ok()) {
                match event {
                    AnyToDeviceEvent::KeyVerificationStart(e) => {
                        if let Some(Verification::SasV1(sas)) = client
                            .encryption()
                            .get_verification(&e.sender, e.content.transaction_id.as_str())
                            .await
                        {
                            println!(
                                "Starting verification with {} {}",
                                &sas.other_device().user_id(),
                                &sas.other_device().device_id()
                            );
                            print_devices(&e.sender, client).await;
                            sas.accept().await.unwrap();
                        }
                    }

                    AnyToDeviceEvent::KeyVerificationKey(e) => {
                        if let Some(Verification::SasV1(sas)) = client
                            .encryption()
                            .get_verification(&e.sender, e.content.transaction_id.as_str())
                            .await
                        {
                            tokio::spawn(wait_for_confirmation((*client).clone(), sas));
                        }
                    }

                    AnyToDeviceEvent::KeyVerificationMac(e) => {
                        if let Some(Verification::SasV1(sas)) = client
                            .encryption()
                            .get_verification(&e.sender, e.content.transaction_id.as_str())
                            .await
                        {
                            if sas.is_done() {
                                print_result(&sas);
                                print_devices(&e.sender, client).await;
                            }
                        }
                    }

                    _ => (),
                }
            }

            if !initial.load(Ordering::SeqCst) {
                for (_room_id, room_info) in response.rooms.join {
                    for event in
                        room_info.timeline.events.iter().filter_map(|e| e.event.deserialize().ok())
                    {
                        if let AnySyncRoomEvent::Message(event) = event {
                            match event {
                                AnySyncMessageEvent::RoomMessage(m) => {
                                    if let MessageType::VerificationRequest(_) = &m.content.msgtype
                                    {
                                        let request = client
                                            .encryption()
                                            .get_verification_request(&m.sender, &m.event_id)
                                            .await
                                            .expect("Request object wasn't created");

                                        request
                                            .accept()
                                            .await
                                            .expect("Can't accept verification request");
                                    }
                                }
                                AnySyncMessageEvent::KeyVerificationKey(e) => {
                                    if let Some(Verification::SasV1(sas)) = client
                                        .encryption()
                                        .get_verification(
                                            &e.sender,
                                            e.content.relates_to.event_id.as_str(),
                                        )
                                        .await
                                    {
                                        tokio::spawn(wait_for_confirmation((*client).clone(), sas));
                                    }
                                }
                                AnySyncMessageEvent::KeyVerificationMac(e) => {
                                    if let Some(Verification::SasV1(sas)) = client
                                        .encryption()
                                        .get_verification(
                                            &e.sender,
                                            e.content.relates_to.event_id.as_str(),
                                        )
                                        .await
                                    {
                                        if sas.is_done() {
                                            print_result(&sas);
                                            print_devices(&e.sender, client).await;
                                        }
                                    }
                                }
                                _ => (),
                            }
                        }
                    }
                }
            }

            initial.store(false, Ordering::SeqCst);

            LoopCtrl::Continue
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
