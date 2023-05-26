use std::sync::{Arc, RwLock};

use futures_util::StreamExt;
use matrix_sdk::{
    encryption::{
        identities::UserIdentity,
        verification::{SasState, SasVerification, VerificationRequest},
        Encryption,
    },
    ruma::events::{key::verification::VerificationMethod, AnyToDeviceEvent},
};

use super::RUNTIME;
use crate::error::ClientError;

pub struct SessionVerificationEmoji {
    symbol: String,
    description: String,
}

#[uniffi::export]
impl SessionVerificationEmoji {
    pub fn symbol(&self) -> String {
        self.symbol.clone()
    }

    pub fn description(&self) -> String {
        self.description.clone()
    }
}

pub trait SessionVerificationControllerDelegate: Sync + Send {
    fn did_accept_verification_request(&self);
    fn did_start_sas_verification(&self);
    fn did_receive_verification_data(&self, data: Vec<Arc<SessionVerificationEmoji>>);
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

#[uniffi::export]
impl SessionVerificationController {
    pub fn is_verified(&self) -> bool {
        self.user_identity.is_verified()
    }

    pub fn set_delegate(&self, delegate: Option<Box<dyn SessionVerificationControllerDelegate>>) {
        *self.delegate.write().unwrap() = delegate;
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl SessionVerificationController {
    pub async fn request_verification(&self) -> Result<(), ClientError> {
        let methods = vec![VerificationMethod::SasV1];
        let verification_request = self
            .user_identity
            .request_verification_with_methods(methods)
            .await
            .map_err(anyhow::Error::from)?;
        *self.verification_request.write().unwrap() = Some(verification_request);

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
                    RUNTIME.spawn(Self::listen_to_changes(delegate, verification));
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
            // TODO: Use the changes stream for this as well once we expose
            // VerificationRequest::changes() in the main crate.
            AnyToDeviceEvent::KeyVerificationStart(event) => {
                if !self.is_transaction_id_valid(event.content.transaction_id.to_string()) {
                    return;
                }
                if let Some(verification) = self
                    .encryption
                    .get_verification(
                        self.user_identity.user_id(),
                        event.content.transaction_id.as_str(),
                    )
                    .await
                {
                    if let Some(sas_verification) = verification.sas() {
                        *self.sas_verification.write().unwrap() = Some(sas_verification.clone());

                        if sas_verification.accept().await.is_ok() {
                            if let Some(delegate) = &*self.delegate.read().unwrap() {
                                delegate.did_start_sas_verification()
                            }

                            let delegate = self.delegate.clone();
                            RUNTIME.spawn(Self::listen_to_changes(delegate, sas_verification));
                        } else if let Some(delegate) = &*self.delegate.read().unwrap() {
                            delegate.did_fail()
                        }
                    }
                }
            }
            AnyToDeviceEvent::KeyVerificationReady(event) => {
                if !self.is_transaction_id_valid(event.content.transaction_id.to_string()) {
                    return;
                }

                if let Some(delegate) = &*self.delegate.read().unwrap() {
                    delegate.did_accept_verification_request()
                }
            }
            _ => (),
        }
    }

    fn is_transaction_id_valid(&self, transaction_id: String) -> bool {
        match &*self.verification_request.read().unwrap() {
            Some(verification) => verification.flow_id() == transaction_id,
            None => false,
        }
    }

    async fn listen_to_changes(delegate: Delegate, sas: SasVerification) {
        let mut stream = sas.changes();

        while let Some(state) = stream.next().await {
            match state {
                SasState::KeysExchanged { emojis, decimals: _ } => {
                    // TODO: If emojis is None, decimals should be used.
                    if let Some(emojis) = emojis {
                        if let Some(delegate) = &*delegate.read().unwrap() {
                            let emojis = emojis
                                .emojis
                                .iter()
                                .map(|e| {
                                    Arc::new(SessionVerificationEmoji {
                                        symbol: e.symbol.to_owned(),
                                        description: e.description.to_owned(),
                                    })
                                })
                                .collect::<Vec<_>>();

                            delegate.did_receive_verification_data(emojis);
                        }
                    } else if let Some(delegate) = &*delegate.read().unwrap() {
                        delegate.did_fail()
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
                SasState::Started { .. } | SasState::Accepted { .. } | SasState::Confirmed => (),
            }
        }
    }
}
