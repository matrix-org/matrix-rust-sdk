use std::{
    io::{self, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::Result;
use clap::Parser;
use matrix_sdk::{
    self,
    config::SyncSettings,
    encryption::verification::{format_emojis, SasVerification, Verification},
    ruma::{
        events::{
            room::message::MessageType, AnySyncMessageLikeEvent, AnySyncTimelineEvent,
            AnyToDeviceEvent, SyncMessageLikeEvent,
        },
        UserId,
    },
    Client, LoopCtrl,
};
use url::Url;

async fn wait_for_confirmation(client: Client, sas: SasVerification) {
    let emoji = sas.emoji().expect("The emoji should be available now");

    println!("\nDo the emojis match: \n{}", format_emojis(emoji));
    print!("Confirm with `yes` or cancel with `no`: ");
    std::io::stdout().flush().expect("We should be able to flush stdout");

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
            device.is_verified()
        );
    }
}

async fn sync(client: Client) -> matrix_sdk::Result<()> {
    let client_ref = &client;
    let initial_sync = Arc::new(AtomicBool::from(true));
    let initial_ref = &initial_sync;

    client
        .sync_with_callback(SyncSettings::new(), |response| async move {
            let client = &client_ref;
            let initial = &initial_ref;

            if response.is_err() {
                return LoopCtrl::Break;
            }

            let sync_response = response.ok().unwrap();

            for event in sync_response.to_device.events.iter().filter_map(|e| e.deserialize().ok())
            {
                match event {
                    AnyToDeviceEvent::KeyVerificationRequest(e) => {
                        let request = client
                            .encryption()
                            .get_verification_request(&e.sender, &e.content.transaction_id)
                            .await
                            .expect("Request object wasn't created");

                        request.accept().await.expect("Can't accept verification request");
                    }
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

                    AnyToDeviceEvent::KeyVerificationDone(e) => {
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
                for (_room_id, room_info) in sync_response.rooms.join {
                    for event in
                        room_info.timeline.events.iter().filter_map(|e| e.event.deserialize().ok())
                    {
                        if let AnySyncTimelineEvent::MessageLike(event) = event {
                            match event {
                                AnySyncMessageLikeEvent::RoomMessage(
                                    SyncMessageLikeEvent::Original(m),
                                ) => {
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
                                AnySyncMessageLikeEvent::KeyVerificationStart(
                                    SyncMessageLikeEvent::Original(e),
                                ) => {
                                    if let Some(Verification::SasV1(sas)) = client
                                        .encryption()
                                        .get_verification(
                                            &e.sender,
                                            e.content.relates_to.event_id.as_str(),
                                        )
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
                                AnySyncMessageLikeEvent::KeyVerificationKey(
                                    SyncMessageLikeEvent::Original(e),
                                ) => {
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
                                AnySyncMessageLikeEvent::KeyVerificationDone(
                                    SyncMessageLikeEvent::Original(e),
                                ) => {
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

#[derive(Parser, Debug)]
struct Cli {
    /// The homeserver to connect to.
    #[clap(value_parser)]
    homeserver: Url,

    /// The user name that should be used for the login.
    #[clap(value_parser)]
    user_name: String,

    /// The password that should be used for the login.
    #[clap(value_parser)]
    password: String,

    /// Set the proxy that should be used for the connection.
    #[clap(short, long)]
    proxy: Option<Url>,

    /// Enable verbose logging output.
    #[clap(short, long, action)]
    verbose: bool,
}

async fn login(cli: Cli) -> Result<Client> {
    let builder = Client::builder().homeserver_url(cli.homeserver);

    let builder = if let Some(proxy) = cli.proxy { builder.proxy(proxy) } else { builder };

    let client = builder.build().await?;

    client
        .login_username(&cli.user_name, &cli.password)
        .initial_device_display_name("rust-sdk")
        .send()
        .await?;

    Ok(client)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.verbose {
        tracing_subscriber::fmt::init();
    }

    let client = login(cli).await?;

    sync(client).await?;

    Ok(())
}
