use std::sync::{Arc, RwLock};

use futures_util::StreamExt;
use matrix_sdk::{
    encryption::{
        identities::UserIdentity,
        verification::{SasState, SasVerification, VerificationRequest, VerificationRequestState},
        Encryption,
    },
    ruma::events::{key::verification::VerificationMethod, AnyToDeviceEvent},
};
use ruma::UserId;
use tracing::{error, info};

use super::RUNTIME;
use crate::error::ClientError;

#[derive(uniffi::Object)]
pub struct SessionVerificationEmoji {
    symbol: String,
    description: String,
}

#[matrix_sdk_ffi_macros::export]
impl SessionVerificationEmoji {
    pub fn symbol(&self) -> String {
        self.symbol.clone()
    }

    pub fn description(&self) -> String {
        self.description.clone()
    }
}

#[derive(uniffi::Enum)]
pub enum SessionVerificationData {
    Emojis { emojis: Vec<Arc<SessionVerificationEmoji>>, indices: Vec<u8> },
    Decimals { values: Vec<u16> },
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait SessionVerificationControllerDelegate: Sync + Send {
    fn did_receive_verification_request(&self, sender_id: String, flow_id: String);
    fn did_accept_verification_request(&self);
    fn did_start_sas_verification(&self);
    fn did_receive_verification_data(&self, data: SessionVerificationData);
    fn did_fail(&self);
    fn did_cancel(&self);
    fn did_finish(&self);
}

pub type Delegate = Arc<RwLock<Option<Box<dyn SessionVerificationControllerDelegate>>>>;

#[derive(Clone, uniffi::Object)]
pub struct SessionVerificationController {
    encryption: Encryption,
    user_identity: UserIdentity,
    delegate: Delegate,
    verification_request: Arc<RwLock<Option<VerificationRequest>>>,
    sas_verification: Arc<RwLock<Option<SasVerification>>>,
}

#[matrix_sdk_ffi_macros::export]
impl SessionVerificationController {
    pub fn set_delegate(&self, delegate: Option<Box<dyn SessionVerificationControllerDelegate>>) {
        *self.delegate.write().unwrap() = delegate;
    }

    /// Set this particular request as the currently active one and register for events pertaining it.
    /// * `sender_id` - The user requesting verification.
    /// * `flow_id` - - The ID that uniquely identifies the verification flow.
    pub async fn acknowledge_verification_request(&self, sender_id: String, flow_id: String) {
        let sender_id = UserId::parse(sender_id.clone())?;

        let verification_request = self
            .encryption
            .get_verification_request(&sender_id, flow_id)
            .await
            .ok_or(ClientError::new("Unknown session verification request"))?;

        *self.verification_request.write().unwrap() = Some(verification_request.clone());

        RUNTIME.spawn(Self::listen_to_verification_request_changes(
            verification_request,
            self.sas_verification.clone(),
            self.delegate.clone(),
        ));

        Ok(())
    }

    /// Accept the previously acknowledged verification request
    pub async fn accept_verification_request(&self) -> Result<(), ClientError> {
        let verification_request = self.verification_request.read().unwrap().clone();

        if let Some(verification_request) = verification_request {
            verification_request.accept().await?;
        }

        Ok(())
    }

    /// Accept the previously acknowledged verification request
    pub async fn accept_verification_request(&self) -> Result<(), ClientError> {
        let verification_request = self.verification_request.read().unwrap().clone();

        if let Some(verification_request) = verification_request {
            let methods = vec![VerificationMethod::SasV1];
            verification_request.accept_with_methods(methods).await?;
        }

        Ok(())
    }

    /// Request verification for the current device
    pub async fn request_verification(&self) -> Result<(), ClientError> {
        let methods = vec![VerificationMethod::SasV1];
        let verification_request = self
            .user_identity
            .request_verification_with_methods(methods)
            .await
            .map_err(anyhow::Error::from)?;

        *self.verification_request.write().unwrap() = Some(verification_request.clone());

        RUNTIME.spawn(Self::listen_to_verification_request_changes(
            verification_request,
            self.sas_verification.clone(),
            self.delegate.clone(),
        ));

        Ok(())
    }

    pub async fn start_sas_verification(&self) -> Result<(), ClientError> {
        let verification_request = self.verification_request.read().unwrap().clone();

        if let Some(verification) = verification_request {
            match verification.start_sas().await {
                Ok(Some(verification)) => {
                    *self.sas_verification.write().unwrap() = Some(verification.clone());

                    if let Some(delegate) = &*self.delegate.read().unwrap() {
                        delegate.did_start_sas_verification()
                    }

                    let delegate = self.delegate.clone();
                    RUNTIME.spawn(Self::listen_to_sas_verification_changes(verification, delegate));
                }
                _ => {
                    if let Some(delegate) = &*self.delegate.read().unwrap() {
                        delegate.did_fail()
                    }
                }
            }
        }

        Ok(())
    }

    pub async fn approve_verification(&self) -> Result<(), ClientError> {
        let sas_verification = self.sas_verification.read().unwrap().clone();
        if let Some(sas_verification) = sas_verification {
            sas_verification.confirm().await?;
        }

        Ok(())
    }

    pub async fn decline_verification(&self) -> Result<(), ClientError> {
        let sas_verification = self.sas_verification.read().unwrap().clone();
        if let Some(sas_verification) = sas_verification {
            sas_verification.mismatch().await?;
        }

        Ok(())
    }

    pub async fn cancel_verification(&self) -> Result<(), ClientError> {
        let verification_request = self.verification_request.read().unwrap().clone();
        if let Some(verification) = verification_request {
            verification.cancel().await?;
        }

        Ok(())
    }
}

impl SessionVerificationController {
    pub(crate) fn new(encryption: Encryption, user_identity: UserIdentity) -> Self {
        SessionVerificationController {
            encryption,
            user_identity,
            delegate: Arc::new(RwLock::new(None)),
            verification_request: Arc::new(RwLock::new(None)),
            sas_verification: Arc::new(RwLock::new(None)),
        }
    }

    pub(crate) async fn process_to_device_message(&self, event: AnyToDeviceEvent) {
        match event {
            AnyToDeviceEvent::KeyVerificationRequest(event) => {
                info!("Received verification request: {:}", event.sender);

                let Some(request) = self
                    .encryption
                    .get_verification_request(&event.sender, &event.content.transaction_id)
                    .await
                else {
                    error!("Failed retrieving verification request");
                    return;
                };

                if !request.is_self_verification() {
                    info!("Received non-self verification request. Ignoring.");
                    return;
                }

                if let Some(delegate) = &*self.delegate.read().unwrap() {
                    delegate.did_receive_verification_request(
                        request.other_user_id().into(),
                        request.flow_id().into(),
                    );
                }
            }
            _ => (),
        }
    }

    async fn listen_to_verification_request_changes(
        verification_request: VerificationRequest,
        sas_verification: Arc<RwLock<Option<SasVerification>>>,
        delegate: Delegate,
    ) {
        let mut stream = verification_request.changes();

        while let Some(state) = stream.next().await {
            match state {
                VerificationRequestState::Transitioned { verification } => {
                    let Some(verification) = verification.sas() else {
                        error!("Invalid, non-sas verification flow. Returning.");
                        return;
                    };

                    *sas_verification.write().unwrap() = Some(verification.clone());

                    if verification.accept().await.is_ok() {
                        if let Some(delegate) = &*delegate.read().unwrap() {
                            delegate.did_start_sas_verification()
                        }

                        let delegate = delegate.clone();
                        RUNTIME.spawn(Self::listen_to_sas_verification_changes(
                            verification,
                            delegate,
                        ));
                    } else if let Some(delegate) = &*delegate.read().unwrap() {
                        delegate.did_fail()
                    }
                }
                VerificationRequestState::Ready { .. } => {
                    if let Some(delegate) = &*delegate.read().unwrap() {
                        delegate.did_accept_verification_request()
                    }
                }
                VerificationRequestState::Cancelled(..) => {
                    if let Some(delegate) = &*delegate.read().unwrap() {
                        delegate.did_cancel();
                    }
                }
                _ => {}
            }
        }
    }

    async fn listen_to_sas_verification_changes(sas: SasVerification, delegate: Delegate) {
        let mut stream = sas.changes();

        while let Some(state) = stream.next().await {
            match state {
                SasState::KeysExchanged { emojis, decimals } => {
                    if let Some(delegate) = &*delegate.read().unwrap() {
                        if let Some(emojis) = emojis {
                            delegate.did_receive_verification_data(
                                SessionVerificationData::Emojis {
                                    emojis: emojis
                                        .emojis
                                        .into_iter()
                                        .map(|emoji| {
                                            Arc::new(SessionVerificationEmoji {
                                                symbol: emoji.symbol.to_owned(),
                                                description: emoji.description.to_owned(),
                                            })
                                        })
                                        .collect(),
                                    indices: emojis.indices.to_vec(),
                                },
                            );
                        } else {
                            delegate.did_receive_verification_data(
                                SessionVerificationData::Decimals {
                                    values: vec![decimals.0, decimals.1, decimals.2],
                                },
                            )
                        }
                    }
                }
                SasState::Done { .. } => {
                    if let Some(delegate) = &*delegate.read().unwrap() {
                        delegate.did_finish()
                    }
                    break;
                }
                SasState::Cancelled(_cancel_info) => {
                    // TODO: The cancel_info is usable, we should tell the user why we were
                    // cancelled.
                    if let Some(delegate) = &*delegate.read().unwrap() {
                        delegate.did_cancel()
                    }
                    break;
                }
                SasState::Created { .. }
                | SasState::Started { .. }
                | SasState::Accepted { .. }
                | SasState::Confirmed => (),
            }
        }
    }
}
