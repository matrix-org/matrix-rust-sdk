use std::sync::Arc;

use matrix_sdk::{
    encryption::{
        identities::UserIdentity,
        verification::{SasVerification, VerificationRequest},
    },
    ruma::{
        api::client::sync::sync_events::v3::ToDevice,
        events::{key::verification::VerificationMethod, AnyToDeviceEvent},
    },
};
use parking_lot::RwLock;

use super::RUNTIME;

pub struct SessionVerificationEmoji {
    symbol: String,
    description: String,
}

impl SessionVerificationEmoji {
    pub fn symbol(&self) -> String {
        self.symbol.clone()
    }

    pub fn description(&self) -> String {
        self.description.clone()
    }
}

pub trait SessionVerificationControllerDelegate: Sync + Send {
    fn did_receive_verification_data(&self, data: Vec<Arc<SessionVerificationEmoji>>);
    fn did_fail(&self);
    fn did_cancel(&self);
    fn did_finish(&self);
}

#[derive(Clone)]
pub struct SessionVerificationController {
    user_identity: UserIdentity,
    delegate: Arc<RwLock<Option<Box<dyn SessionVerificationControllerDelegate>>>>,
    verification_request: Arc<RwLock<Option<VerificationRequest>>>,
    sas_verification: Arc<RwLock<Option<SasVerification>>>,
}

impl SessionVerificationController {
    pub fn new(user_identity: UserIdentity) -> Self {
        SessionVerificationController {
            user_identity,
            delegate: Arc::new(RwLock::new(None)),
            verification_request: Arc::new(RwLock::new(None)),
            sas_verification: Arc::new(RwLock::new(None)),
        }
    }

    pub fn set_delegate(&self, delegate: Option<Box<dyn SessionVerificationControllerDelegate>>) {
        *self.delegate.write() = delegate;
    }

    pub fn is_verified(&self) -> bool {
        self.user_identity.verified()
    }

    pub fn request_verification(&self) -> anyhow::Result<()> {
        RUNTIME.block_on(async move {
            let methods = vec![VerificationMethod::SasV1];
            let verification_request =
                self.user_identity.request_verification_with_methods(methods).await?;
            *self.verification_request.write() = Some(verification_request);

            Ok(())
        })
    }

    pub fn approve_verification(&self) -> anyhow::Result<()> {
        RUNTIME.block_on(async move {
            let sas_verification = self.sas_verification.read().clone();
            if let Some(sas_verification) = sas_verification {
                sas_verification.confirm().await?;
            }

            Ok(())
        })
    }

    pub fn decline_verification(&self) -> anyhow::Result<()> {
        RUNTIME.block_on(async move {
            let sas_verification = self.sas_verification.read().clone();
            if let Some(sas_verification) = sas_verification {
                sas_verification.mismatch().await?;
            }

            Ok(())
        })
    }

    pub fn cancel_verification(&self) -> anyhow::Result<()> {
        RUNTIME.block_on(async move {
            let verification_request = self.verification_request.read().clone();
            if let Some(verification) = verification_request {
                verification.cancel().await?;
            }

            Ok(())
        })
    }

    pub async fn process_to_device_messages(&self, to_device: ToDevice) {
        let sas_verification = self.sas_verification.clone();

        for event in to_device.events.into_iter().filter_map(|e| e.deserialize().ok()) {
            match event {
                AnyToDeviceEvent::KeyVerificationReady(event) => {
                    if !self.is_transaction_id_valid(event.content.transaction_id.to_string()) {
                        return;
                    }
                    self.start_sas_verification().await;
                }
                AnyToDeviceEvent::KeyVerificationCancel(event) => {
                    if !self.is_transaction_id_valid(event.content.transaction_id.to_string()) {
                        return;
                    }

                    if let Some(delegate) = &*self.delegate.read() {
                        delegate.did_cancel()
                    }
                }
                AnyToDeviceEvent::KeyVerificationKey(event) => {
                    if !self.is_transaction_id_valid(event.content.transaction_id.to_string()) {
                        return;
                    }

                    if let Some(sas_verification) = &*sas_verification.read() {
                        if let Some(emojis) = sas_verification.emoji() {
                            if let Some(delegate) = &*self.delegate.read() {
                                let emojis = emojis
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
                        } else if let Some(delegate) = &*self.delegate.read() {
                            delegate.did_fail()
                        }
                    } else if let Some(delegate) = &*self.delegate.read() {
                        delegate.did_fail()
                    }
                }
                AnyToDeviceEvent::KeyVerificationDone(event) => {
                    if !self.is_transaction_id_valid(event.content.transaction_id.to_string()) {
                        return;
                    }

                    if let Some(delegate) = &*self.delegate.read() {
                        delegate.did_finish()
                    }
                }
                _ => (),
            }
        }
    }

    fn is_transaction_id_valid(&self, transaction_id: String) -> bool {
        if let Some(verification) = &*self.verification_request.read() {
            return verification.flow_id() == transaction_id;
        }

        false
    }

    async fn start_sas_verification(&self) {
        let verification_request = self.verification_request.read().clone();
        if let Some(verification) = verification_request {
            match verification.start_sas().await {
                Ok(verification) => {
                    *self.sas_verification.write() = verification;
                }
                Err(_) => {
                    if let Some(delegate) = &*self.delegate.read() {
                        delegate.did_fail()
                    }
                }
            }
        }
    }
}
