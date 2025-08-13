use std::sync::{Arc, RwLock};

use futures_util::StreamExt;
use matrix_sdk::{
    encryption::{
        identities::UserIdentity,
        verification::{SasState, SasVerification, VerificationRequest, VerificationRequestState},
        Encryption,
    },
    ruma::events::key::verification::VerificationMethod,
    Account,
};
use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm};
use ruma::UserId;
use tracing::{error, warn};

use crate::{
    client::UserProfile, error::ClientError, runtime::get_runtime_handle, utils::Timestamp,
};

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

/// Details about the incoming verification request
#[derive(uniffi::Record)]
pub struct SessionVerificationRequestDetails {
    sender_profile: UserProfile,
    flow_id: String,
    device_id: String,
    device_display_name: Option<String>,
    /// First time this device was seen in milliseconds since epoch.
    first_seen_timestamp: Timestamp,
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait SessionVerificationControllerDelegate: SyncOutsideWasm + SendOutsideWasm {
    fn did_receive_verification_request(&self, details: SessionVerificationRequestDetails);
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
    account: Account,
    delegate: Delegate,
    verification_request: Arc<RwLock<Option<VerificationRequest>>>,
    sas_verification: Arc<RwLock<Option<SasVerification>>>,
}

#[matrix_sdk_ffi_macros::export]
impl SessionVerificationController {
    pub fn set_delegate(&self, delegate: Option<Box<dyn SessionVerificationControllerDelegate>>) {
        *self.delegate.write().unwrap() = delegate;
    }

    /// Set this particular request as the currently active one and register for
    /// events pertaining it.
    /// * `sender_id` - The user requesting verification.
    /// * `flow_id` - - The ID that uniquely identifies the verification flow.
    pub async fn acknowledge_verification_request(
        &self,
        sender_id: String,
        flow_id: String,
    ) -> Result<(), ClientError> {
        let sender_id = UserId::parse(sender_id.clone())?;

        let verification_request = self
            .encryption
            .get_verification_request(&sender_id, flow_id)
            .await
            .ok_or(ClientError::from_str("Unknown session verification request", None))?;

        self.set_ongoing_verification_request(verification_request)
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
    pub async fn request_device_verification(&self) -> Result<(), ClientError> {
        let methods = vec![VerificationMethod::SasV1];
        let verification_request =
            self.user_identity.request_verification_with_methods(methods).await?;

        self.set_ongoing_verification_request(verification_request)
    }

    /// Request verification for the given user
    pub async fn request_user_verification(&self, user_id: String) -> Result<(), ClientError> {
        let user_id = UserId::parse(user_id)?;

        let user_identity = self
            .encryption
            .get_user_identity(&user_id)
            .await?
            .ok_or(ClientError::from_str("Unknown user identity", None))?;

        if user_identity.is_verified() {
            return Err(ClientError::from_str("User is already verified", None));
        }

        let methods = vec![VerificationMethod::SasV1];

        let verification_request = user_identity.request_verification_with_methods(methods).await?;

        self.set_ongoing_verification_request(verification_request)
    }

    /// Transition the current verification request into a SAS verification
    /// flow.
    pub async fn start_sas_verification(&self) -> Result<(), ClientError> {
        let verification_request = self.verification_request.read().unwrap().clone();

        let Some(verification_request) = verification_request else {
            return Err(ClientError::from_str("Verification request missing.", None));
        };

        match verification_request.start_sas().await {
            Ok(Some(verification)) => {
                *self.sas_verification.write().unwrap() = Some(verification.clone());

                if let Some(delegate) = &*self.delegate.read().unwrap() {
                    delegate.did_start_sas_verification()
                }

                let delegate = self.delegate.clone();
                get_runtime_handle()
                    .spawn(Self::listen_to_sas_verification_changes(verification, delegate));
            }
            _ => {
                if let Some(delegate) = &*self.delegate.read().unwrap() {
                    delegate.did_fail()
                }
            }
        }

        Ok(())
    }

    /// Confirm that the short auth strings match on both sides.
    pub async fn approve_verification(&self) -> Result<(), ClientError> {
        let sas_verification = self.sas_verification.read().unwrap().clone();

        let Some(sas_verification) = sas_verification else {
            return Err(ClientError::from_str("SAS verification missing", None));
        };

        Ok(sas_verification.confirm().await?)
    }

    /// Reject the short auth string
    pub async fn decline_verification(&self) -> Result<(), ClientError> {
        let sas_verification = self.sas_verification.read().unwrap().clone();

        let Some(sas_verification) = sas_verification else {
            return Err(ClientError::from_str("SAS verification missing", None));
        };

        Ok(sas_verification.mismatch().await?)
    }

    /// Cancel the current verification request
    pub async fn cancel_verification(&self) -> Result<(), ClientError> {
        let verification_request = self.verification_request.read().unwrap().clone();

        let Some(verification_request) = verification_request else {
            return Err(ClientError::from_str("Verification request missing.", None));
        };

        Ok(verification_request.cancel().await?)
    }
}

impl SessionVerificationController {
    pub(crate) fn new(
        encryption: Encryption,
        user_identity: UserIdentity,
        account: Account,
    ) -> Self {
        SessionVerificationController {
            encryption,
            user_identity,
            account,
            delegate: Arc::new(RwLock::new(None)),
            verification_request: Arc::new(RwLock::new(None)),
            sas_verification: Arc::new(RwLock::new(None)),
        }
    }

    /// Ask the controller to process an incoming request based on the sender
    /// and flow identifier. It will fetch the request, verify that it's in the
    /// correct state and then and notify the delegate.
    pub(crate) async fn process_incoming_verification_request(
        &self,
        sender: &UserId,
        flow_id: impl AsRef<str>,
    ) {
        if sender != self.user_identity.user_id() {
            if let Some(status) = self.encryption.cross_signing_status().await {
                if !status.is_complete() {
                    warn!(
                        "Cannot verify other users until our own device's cross-signing status \
                         is complete: {status:?}"
                    );
                    return;
                }
            }
        }

        let Some(request) = self.encryption.get_verification_request(sender, flow_id).await else {
            error!("Failed retrieving verification request");
            return;
        };

        let VerificationRequestState::Requested { other_device_data, .. } = request.state() else {
            error!("Received verification request event but the request is in the wrong state.");
            return;
        };

        let Ok(sender_profile) = UserProfile::fetch(&self.account, sender).await else {
            error!("Failed fetching user profile for verification request");
            return;
        };

        if let Some(delegate) = &*self.delegate.read().unwrap() {
            delegate.did_receive_verification_request(SessionVerificationRequestDetails {
                sender_profile,
                flow_id: request.flow_id().into(),
                device_id: other_device_data.device_id().into(),
                device_display_name: other_device_data.display_name().map(str::to_string),
                first_seen_timestamp: other_device_data.first_time_seen_ts().into(),
            });
        }
    }

    fn set_ongoing_verification_request(
        &self,
        verification_request: VerificationRequest,
    ) -> Result<(), ClientError> {
        if let Some(ongoing_verification_request) =
            self.verification_request.read().unwrap().clone()
        {
            if !ongoing_verification_request.is_done()
                && !ongoing_verification_request.is_cancelled()
            {
                return Err(ClientError::from_str(
                    "There is another verification flow ongoing.",
                    None,
                ));
            }
        }

        *self.verification_request.write().unwrap() = Some(verification_request.clone());

        get_runtime_handle().spawn(Self::listen_to_verification_request_changes(
            verification_request,
            self.sas_verification.clone(),
            self.delegate.clone(),
        ));

        Ok(())
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
                        get_runtime_handle().spawn(Self::listen_to_sas_verification_changes(
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
