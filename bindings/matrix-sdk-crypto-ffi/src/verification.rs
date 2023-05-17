use std::sync::Arc;

use base64::{
    alphabet,
    engine::{general_purpose, GeneralPurpose},
    Engine,
};
use futures_util::{Stream, StreamExt};
use matrix_sdk_crypto::{
    matrix_sdk_qrcode::QrVerificationData, CancelInfo as RustCancelInfo, QrVerification as InnerQr,
    QrVerificationState, Sas as InnerSas, SasState as RustSasState,
    Verification as InnerVerification, VerificationRequest as InnerVerificationRequest,
    VerificationRequestState as RustVerificationRequestState,
};
use ruma::events::key::verification::VerificationMethod;
use tokio::runtime::Handle;

use crate::{CryptoStoreError, OutgoingVerificationRequest, SignatureUploadRequest};

const STANDARD_NO_PAD: GeneralPurpose =
    GeneralPurpose::new(&alphabet::STANDARD, general_purpose::NO_PAD);

/// Listener that will be passed over the FFI to report changes to a SAS
/// verification.
pub trait SasListener: Send {
    /// The callback that should be called on the Rust side
    ///
    /// # Arguments
    ///
    /// * `state` - The current state of the SAS verification.
    fn on_change(&self, state: SasState);
}

/// An Enum describing the state the SAS verification is in.
pub enum SasState {
    /// The verification has been started, the protocols that should be used
    /// have been proposed and can be accepted.
    Started,
    /// The verification has been accepted and both sides agreed to a set of
    /// protocols that will be used for the verification process.
    Accepted,
    /// The public keys have been exchanged and the short auth string can be
    /// presented to the user.
    KeysExchanged {
        /// The emojis that represent the short auth string, will be `None` if
        /// the emoji SAS method wasn't one of accepted protocols.
        emojis: Option<Vec<i32>>,
        /// The list of decimals that represent the short auth string.
        decimals: Vec<i32>,
    },
    /// The verification process has been confirmed from our side, we're waiting
    /// for the other side to confirm as well.
    Confirmed,
    /// The verification process has been successfully concluded.
    Done,
    /// The verification process has been cancelled.
    Cancelled {
        /// Information about the reason of the cancellation.
        cancel_info: CancelInfo,
    },
}

impl From<RustSasState> for SasState {
    fn from(s: RustSasState) -> Self {
        match s {
            RustSasState::Started { .. } => Self::Started,
            RustSasState::Accepted { .. } => Self::Accepted,
            RustSasState::KeysExchanged { emojis, decimals } => Self::KeysExchanged {
                emojis: emojis.map(|e| e.indices.map(|i| i as i32).to_vec()),
                decimals: [decimals.0.into(), decimals.1.into(), decimals.2.into()].to_vec(),
            },
            RustSasState::Confirmed => Self::Confirmed,
            RustSasState::Done { .. } => Self::Done,
            RustSasState::Cancelled(c) => Self::Cancelled { cancel_info: c.into() },
        }
    }
}

/// Enum representing the different verification flows we support.
#[derive(uniffi::Object)]
pub struct Verification {
    pub(crate) inner: InnerVerification,
    pub(crate) runtime: Handle,
}

#[uniffi::export]
impl Verification {
    /// Try to represent the `Verification` as an `Sas` verification object,
    /// returns `None` if the verification is not a `Sas` verification.
    pub fn as_sas(&self) -> Option<Arc<Sas>> {
        if let InnerVerification::SasV1(sas) = &self.inner {
            Some(Sas { inner: sas.to_owned(), runtime: self.runtime.to_owned() }.into())
        } else {
            None
        }
    }

    /// Try to represent the `Verification` as an `QrCode` verification object,
    /// returns `None` if the verification is not a `QrCode` verification.
    pub fn as_qr(&self) -> Option<Arc<QrCode>> {
        if let InnerVerification::QrV1(qr) = &self.inner {
            Some(QrCode { inner: qr.to_owned(), runtime: self.runtime.to_owned() }.into())
        } else {
            None
        }
    }
}

/// The `m.sas.v1` verification flow.
#[derive(uniffi::Object)]
pub struct Sas {
    pub(crate) inner: InnerSas,
    pub(crate) runtime: Handle,
}

#[uniffi::export]
impl Sas {
    /// Get the user id of the other side.
    pub fn other_user_id(&self) -> String {
        self.inner.other_user_id().to_string()
    }

    /// Get the device ID of the other side.
    pub fn other_device_id(&self) -> String {
        self.inner.other_device_id().to_string()
    }

    /// Get the unique ID that identifies this SAS verification flow.
    pub fn flow_id(&self) -> String {
        self.inner.flow_id().as_str().to_owned()
    }

    /// Get the room id if the verification is happening inside a room.
    pub fn room_id(&self) -> Option<String> {
        self.inner.room_id().map(|r| r.to_string())
    }

    /// Is the SAS flow done.
    pub fn is_done(&self) -> bool {
        self.inner.is_done()
    }

    /// Did we initiate the verification flow.
    pub fn we_started(&self) -> bool {
        self.inner.we_started()
    }

    /// Accept that we're going forward with the short auth string verification.
    pub fn accept(&self) -> Option<OutgoingVerificationRequest> {
        self.inner.accept().map(|r| r.into())
    }

    /// Confirm a verification was successful.
    ///
    /// This method should be called if a short auth string should be confirmed
    /// as matching.
    pub fn confirm(&self) -> Result<Option<ConfirmVerificationResult>, CryptoStoreError> {
        let (requests, signature_request) = self.runtime.block_on(self.inner.confirm())?;

        let requests = requests.into_iter().map(|r| r.into()).collect();

        Ok(Some(ConfirmVerificationResult {
            requests,
            signature_request: signature_request.map(|s| s.into()),
        }))
    }

    /// Cancel the SAS verification using the given cancel code.
    ///
    /// # Arguments
    ///
    /// * `cancel_code` - The error code for why the verification was cancelled,
    /// manual cancellatio usually happens with `m.user` cancel code. The full
    /// list of cancel codes can be found in the [spec]
    ///
    /// [spec]: https://spec.matrix.org/unstable/client-server-api/#mkeyverificationcancel
    pub fn cancel(&self, cancel_code: String) -> Option<OutgoingVerificationRequest> {
        self.inner.cancel_with_code(cancel_code.into()).map(|r| r.into())
    }

    /// Get a list of emoji indices of the emoji representation of the short
    /// auth string.
    ///
    /// *Note*: A SAS verification needs to be started and in the presentable
    /// state for this to return the list of emoji indices, otherwise returns
    /// `None`.
    pub fn get_emoji_indices(&self) -> Option<Vec<i32>> {
        self.inner.emoji_index().map(|v| v.iter().map(|i| (*i).into()).collect())
    }

    /// Get the decimal representation of the short auth string.
    ///
    /// *Note*: A SAS verification needs to be started and in the presentable
    /// state for this to return the list of decimals, otherwise returns
    /// `None`.
    pub fn get_decimals(&self) -> Option<Vec<i32>> {
        self.inner.decimals().map(|v| [v.0.into(), v.1.into(), v.2.into()].to_vec())
    }

    /// Set a listener for changes in the SAS verification process.
    ///
    /// The given callback will be called whenever the state changes.
    ///
    /// This method can be used to react to changes in the state of the
    /// verification process, or rather the method can be used to handle
    /// each step of the verification process.
    ///
    /// This method will spawn a tokio task on the Rust side, once we reach the
    /// Done or Cancelled state, the task will stop listening for changes.
    ///
    /// # Flowchart
    ///
    /// The flow of the verification process is pictured bellow. Please note
    /// that the process can be cancelled at each step of the process.
    /// Either side can cancel the process.
    ///
    /// ```text
    ///                ┌───────┐
    ///                │Started│
    ///                └───┬───┘
    ///                    │
    ///               ┌────⌄───┐
    ///               │Accepted│
    ///               └────┬───┘
    ///                    │
    ///            ┌───────⌄──────┐
    ///            │Keys Exchanged│
    ///            └───────┬──────┘
    ///                    │
    ///            ________⌄________
    ///           ╱                 ╲       ┌─────────┐
    ///          ╱   Does the short  ╲______│Cancelled│
    ///          ╲ auth string match ╱ no   └─────────┘
    ///           ╲_________________╱
    ///                    │yes
    ///                    │
    ///               ┌────⌄────┐
    ///               │Confirmed│
    ///               └────┬────┘
    ///                    │
    ///                ┌───⌄───┐
    ///                │  Done │
    ///                └───────┘
    /// ```
    pub fn set_changes_listener(&self, listener: Box<dyn SasListener>) {
        let stream = self.inner.changes();

        self.runtime.spawn(Self::changes_listener(stream, listener));
    }

    /// Get the current state of the SAS verification process.
    pub fn state(&self) -> SasState {
        self.inner.state().into()
    }
}

impl Sas {
    async fn changes_listener(
        mut stream: impl Stream<Item = RustSasState> + std::marker::Unpin,
        listener: Box<dyn SasListener>,
    ) {
        while let Some(state) = stream.next().await {
            // If we receive a done or a cancelled state we're at the end of our road, we
            // break out of the loop to deallocate the stream and finish the
            // task.
            let should_break =
                matches!(state, RustSasState::Done { .. } | RustSasState::Cancelled { .. });

            listener.on_change(state.into());

            if should_break {
                break;
            }
        }
    }
}

/// Listener that will be passed over the FFI to report changes to a QrCode
/// verification.
pub trait QrCodeListener: Send {
    /// The callback that should be called on the Rust side
    ///
    /// # Arguments
    ///
    /// * `state` - The current state of the QrCode verification.
    fn on_change(&self, state: QrCodeState);
}

/// An Enum describing the state the QrCode verification is in.
pub enum QrCodeState {
    /// The QR verification has been started.
    Started,
    /// The QR verification has been scanned by the other side.
    Scanned,
    /// The scanning of the QR code has been confirmed by us.
    Confirmed,
    /// We have successfully scanned the QR code and are able to send a
    /// reciprocation event.
    Reciprocated,
    /// The verification process has been successfully concluded.
    Done,
    /// The verification process has been cancelled.
    Cancelled {
        /// Information about the reason of the cancellation.
        cancel_info: CancelInfo,
    },
}

impl From<QrVerificationState> for QrCodeState {
    fn from(value: QrVerificationState) -> Self {
        match value {
            QrVerificationState::Started => Self::Started,
            QrVerificationState::Scanned => Self::Scanned,
            QrVerificationState::Confirmed => Self::Confirmed,
            QrVerificationState::Reciprocated => Self::Reciprocated,
            QrVerificationState::Done { .. } => Self::Done,
            QrVerificationState::Cancelled(c) => Self::Cancelled { cancel_info: c.into() },
        }
    }
}

/// The `m.qr_code.scan.v1`, `m.qr_code.show.v1`, and `m.reciprocate.v1`
/// verification flow.
#[derive(uniffi::Object)]
pub struct QrCode {
    pub(crate) inner: InnerQr,
    pub(crate) runtime: Handle,
}

#[uniffi::export]
impl QrCode {
    /// Get the user id of the other side.
    pub fn other_user_id(&self) -> String {
        self.inner.other_user_id().to_string()
    }

    /// Get the device ID of the other side.
    pub fn other_device_id(&self) -> String {
        self.inner.other_device_id().to_string()
    }

    /// Get the unique ID that identifies this QR code verification flow.
    pub fn flow_id(&self) -> String {
        self.inner.flow_id().as_str().to_owned()
    }

    /// Get the room id if the verification is happening inside a room.
    pub fn room_id(&self) -> Option<String> {
        self.inner.room_id().map(|r| r.to_string())
    }

    /// Is the QR code verification done.
    pub fn is_done(&self) -> bool {
        self.inner.is_done()
    }

    /// Has the verification flow been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    /// Did we initiate the verification flow.
    pub fn we_started(&self) -> bool {
        self.inner.we_started()
    }

    /// Get the CancelInfo of this QR code verification object.
    ///
    /// Will be `None` if the flow has not been cancelled.
    pub fn cancel_info(&self) -> Option<CancelInfo> {
        self.inner.cancel_info().map(|c| c.into())
    }

    /// Has the QR verification been scanned by the other side.
    ///
    /// When the verification object is in this state it's required that the
    /// user confirms that the other side has scanned the QR code.
    pub fn has_been_scanned(&self) -> bool {
        self.inner.has_been_scanned()
    }

    /// Have we successfully scanned the QR code and are able to send a
    /// reciprocation event.
    pub fn reciprocated(&self) -> bool {
        self.inner.reciprocated()
    }

    /// Cancel the QR code verification using the given cancel code.
    ///
    /// # Arguments
    ///
    /// * `cancel_code` - The error code for why the verification was cancelled,
    /// manual cancellatio usually happens with `m.user` cancel code. The full
    /// list of cancel codes can be found in the [spec]
    ///
    /// [spec]: https://spec.matrix.org/unstable/client-server-api/#mkeyverificationcancel
    pub fn cancel(&self, cancel_code: String) -> Option<OutgoingVerificationRequest> {
        self.inner.cancel_with_code(cancel_code.into()).map(|r| r.into())
    }

    /// Confirm a verification was successful.
    ///
    /// This method should be called if we want to confirm that the other side
    /// has scanned our QR code.
    pub fn confirm(&self) -> Option<ConfirmVerificationResult> {
        self.inner.confirm_scanning().map(|r| ConfirmVerificationResult {
            requests: vec![r.into()],
            signature_request: None,
        })
    }

    /// Generate data that should be encoded as a QR code.
    ///
    /// This method should be called right before a QR code should be displayed,
    /// the returned data is base64 encoded (without padding) and needs to be
    /// decoded on the other side before it can be put through a QR code
    /// generator.
    pub fn generate_qr_code(&self) -> Option<String> {
        self.inner.to_bytes().map(|data| STANDARD_NO_PAD.encode(data)).ok()
    }

    /// Set a listener for changes in the QrCode verification process.
    ///
    /// The given callback will be called whenever the state changes.
    pub fn set_changes_listener(&self, listener: Box<dyn QrCodeListener>) {
        let stream = self.inner.changes();

        self.runtime.spawn(Self::changes_listener(stream, listener));
    }

    /// Get the current state of the QrCode verification process.
    pub fn state(&self) -> QrCodeState {
        self.inner.state().into()
    }
}

impl QrCode {
    async fn changes_listener(
        mut stream: impl Stream<Item = QrVerificationState> + std::marker::Unpin,
        listener: Box<dyn QrCodeListener>,
    ) {
        while let Some(state) = stream.next().await {
            // If we receive a done or a cancelled state we're at the end of our road, we
            // break out of the loop to deallocate the stream and finish the
            // task.
            let should_break = matches!(
                state,
                QrVerificationState::Done { .. } | QrVerificationState::Cancelled { .. }
            );

            listener.on_change(state.into());

            if should_break {
                break;
            }
        }
    }
}

/// Information on why a verification flow has been cancelled and by whom.
pub struct CancelInfo {
    /// The textual representation of the cancel reason
    pub reason: String,
    /// The code describing the cancel reason
    pub cancel_code: String,
    /// Was the verification flow cancelled by us
    pub cancelled_by_us: bool,
}

impl From<RustCancelInfo> for CancelInfo {
    fn from(c: RustCancelInfo) -> Self {
        Self {
            reason: c.reason().to_owned(),
            cancel_code: c.cancel_code().to_string(),
            cancelled_by_us: c.cancelled_by_us(),
        }
    }
}

/// A result type for starting SAS verifications.
#[derive(uniffi::Record)]
pub struct StartSasResult {
    /// The SAS verification object that got created.
    pub sas: Arc<Sas>,
    /// The request that needs to be sent out to notify the other side that a
    /// SAS verification should start.
    pub request: OutgoingVerificationRequest,
}

/// A result type for scanning QR codes.
#[derive(uniffi::Record)]
pub struct ScanResult {
    /// The QR code verification object that got created.
    pub qr: Arc<QrCode>,
    /// The request that needs to be sent out to notify the other side that a
    /// QR code verification should start.
    pub request: OutgoingVerificationRequest,
}

/// A result type for requesting verifications.
#[derive(uniffi::Record)]
pub struct RequestVerificationResult {
    /// The verification request object that got created.
    pub verification: Arc<VerificationRequest>,
    /// The request that needs to be sent out to notify the other side that
    /// we're requesting verification to begin.
    pub request: OutgoingVerificationRequest,
}

/// A result type for confirming verifications.
#[derive(uniffi::Record)]
pub struct ConfirmVerificationResult {
    /// The requests that needs to be sent out to notify the other side that we
    /// confirmed the verification.
    pub requests: Vec<OutgoingVerificationRequest>,
    /// A request that will upload signatures of the verified device or user, if
    /// the verification is completed and we're able to sign devices or users
    pub signature_request: Option<SignatureUploadRequest>,
}

/// Listener that will be passed over the FFI to report changes to a
/// verification request.
pub trait VerificationRequestListener: Send {
    /// The callback that should be called on the Rust side
    ///
    /// # Arguments
    ///
    /// * `state` - The current state of the verification request.
    fn on_change(&self, state: VerificationRequestState);
}

/// An Enum describing the state the QrCode verification is in.
pub enum VerificationRequestState {
    /// The verification request was sent
    Requested,
    /// The verification request is ready to start a verification flow.
    Ready {
        /// The verification methods supported by the other side.
        their_methods: Vec<String>,

        /// The verification methods supported by the us.
        our_methods: Vec<String>,
    },
    /// The verification flow that was started with this request has finished.
    Done,
    /// The verification process has been cancelled.
    Cancelled {
        /// Information about the reason of the cancellation.
        cancel_info: CancelInfo,
    },
}

/// The verificatoin request object which then can transition into some concrete
/// verification method
#[derive(uniffi::Object)]
pub struct VerificationRequest {
    pub(crate) inner: InnerVerificationRequest,
    pub(crate) runtime: Handle,
}

#[uniffi::export]
impl VerificationRequest {
    /// The id of the other user that is participating in this verification
    /// request.
    pub fn other_user_id(&self) -> String {
        self.inner.other_user().to_string()
    }

    /// The id of the other device that is participating in this verification.
    pub fn other_device_id(&self) -> Option<String> {
        self.inner.other_device_id().map(|d| d.to_string())
    }

    /// Get the unique ID of this verification request
    pub fn flow_id(&self) -> String {
        self.inner.flow_id().as_str().to_owned()
    }

    /// Get the room id if the verification is happening inside a room.
    pub fn room_id(&self) -> Option<String> {
        self.inner.room_id().map(|r| r.to_string())
    }

    /// Has the verification flow that was started with this request finished.
    pub fn is_done(&self) -> bool {
        self.inner.is_done()
    }

    /// Is the verification request ready to start a verification flow.
    pub fn is_ready(&self) -> bool {
        self.inner.is_ready()
    }

    /// Did we initiate the verification request
    pub fn we_started(&self) -> bool {
        self.inner.we_started()
    }

    /// Has the verification request been answered by another device.
    pub fn is_passive(&self) -> bool {
        self.inner.is_passive()
    }

    /// Has the verification flow that been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    /// Get info about the cancellation if the verification request has been
    /// cancelled.
    pub fn cancel_info(&self) -> Option<CancelInfo> {
        self.inner.cancel_info().map(|v| v.into())
    }

    /// Get the supported verification methods of the other side.
    ///
    /// Will be present only if the other side requested the verification or if
    /// we're in the ready state.
    pub fn their_supported_methods(&self) -> Option<Vec<String>> {
        self.inner.their_supported_methods().map(|m| m.iter().map(|m| m.to_string()).collect())
    }

    /// Get our own supported verification methods that we advertised.
    ///
    /// Will be present only we requested the verification or if we're in the
    /// ready state.
    pub fn our_supported_methods(&self) -> Option<Vec<String>> {
        self.inner.our_supported_methods().map(|m| m.iter().map(|m| m.to_string()).collect())
    }

    /// Accept a verification requests that we share with the given user with
    /// the given flow id.
    ///
    /// This will move the verification request into the ready state.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The ID of the user for which we would like to accept the
    /// verification requests.
    ///
    /// * `flow_id` - The ID that uniquely identifies the verification flow.
    ///
    /// * `methods` - A list of verification methods that we want to advertise
    /// as supported.
    pub fn accept(&self, methods: Vec<String>) -> Option<OutgoingVerificationRequest> {
        let methods = methods.into_iter().map(VerificationMethod::from).collect();
        self.inner.accept_with_methods(methods).map(|r| r.into())
    }

    /// Cancel a verification for the given user with the given flow id using
    /// the given cancel code.
    pub fn cancel(&self) -> Option<OutgoingVerificationRequest> {
        self.inner.cancel().map(|r| r.into())
    }

    /// Transition from a verification request into short auth string based
    /// verification.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The ID of the user for which we would like to start the
    /// SAS verification.
    ///
    /// * `flow_id` - The ID of the verification request that initiated the
    /// verification flow.
    pub fn start_sas_verification(&self) -> Result<Option<StartSasResult>, CryptoStoreError> {
        Ok(self.runtime.block_on(self.inner.start_sas())?.map(|(sas, r)| StartSasResult {
            sas: Arc::new(Sas { inner: sas, runtime: self.runtime.clone() }),
            request: r.into(),
        }))
    }

    /// Transition from a verification request into QR code verification.
    ///
    /// This method should be called when one wants to display a QR code so the
    /// other side can scan it and move the QR code verification forward.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The ID of the user for which we would like to start the
    /// QR code verification.
    ///
    /// * `flow_id` - The ID of the verification request that initiated the
    /// verification flow.
    pub fn start_qr_verification(&self) -> Result<Option<Arc<QrCode>>, CryptoStoreError> {
        Ok(self
            .runtime
            .block_on(self.inner.generate_qr_code())?
            .map(|qr| QrCode { inner: qr, runtime: self.runtime.clone() }.into()))
    }

    /// Pass data from a scanned QR code to an active verification request and
    /// transition into QR code verification.
    ///
    /// This requires an active `VerificationRequest` to succeed, returns `None`
    /// if no `VerificationRequest` is found or if the QR code data is invalid.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The ID of the user for which we would like to start the
    /// QR code verification.
    ///
    /// * `flow_id` - The ID of the verification request that initiated the
    /// verification flow.
    ///
    /// * `data` - The data that was extracted from the scanned QR code as an
    /// base64 encoded string, without padding.
    pub fn scan_qr_code(&self, data: String) -> Option<ScanResult> {
        let data = STANDARD_NO_PAD.decode(data).ok()?;
        let data = QrVerificationData::from_bytes(data).ok()?;

        if let Some(qr) = self.runtime.block_on(self.inner.scan_qr_code(data)).ok()? {
            let request = qr.reciprocate()?;

            Some(ScanResult {
                qr: QrCode { inner: qr, runtime: self.runtime.clone() }.into(),
                request: request.into(),
            })
        } else {
            None
        }
    }

    /// Set a listener for changes in the verification request
    ///
    /// The given callback will be called whenever the state changes.
    pub fn set_changes_listener(&self, listener: Box<dyn VerificationRequestListener>) {
        let stream = self.inner.changes();

        self.runtime.spawn(Self::changes_listener(self.inner.to_owned(), stream, listener));
    }

    /// Get the current state of the verification request.
    pub fn state(&self) -> VerificationRequestState {
        Self::convert_verification_request(&self.inner, self.inner.state())
    }
}

impl VerificationRequest {
    fn convert_verification_request(
        request: &InnerVerificationRequest,
        value: RustVerificationRequestState,
    ) -> VerificationRequestState {
        match value {
            // The clients do not need to distinguish `Created` and `Requested` state
            RustVerificationRequestState::Created { .. } => VerificationRequestState::Requested,
            RustVerificationRequestState::Requested { .. } => VerificationRequestState::Requested,
            RustVerificationRequestState::Ready {
                their_methods,
                our_methods,
                other_device_id: _,
            } => VerificationRequestState::Ready {
                their_methods: their_methods.iter().map(|m| m.to_string()).collect(),
                our_methods: our_methods.iter().map(|m| m.to_string()).collect(),
            },
            RustVerificationRequestState::Done => VerificationRequestState::Done,
            RustVerificationRequestState::Transitioned { .. } => {
                let their_methods = request
                    .their_supported_methods()
                    .expect("The transitioned state should know the other side's methods")
                    .into_iter()
                    .map(|m| m.to_string())
                    .collect();
                let our_methods = request
                    .our_supported_methods()
                    .expect("The transitioned state should know our own supported methods")
                    .iter()
                    .map(|m| m.to_string())
                    .collect();
                VerificationRequestState::Ready { their_methods, our_methods }
            }

            RustVerificationRequestState::Cancelled(c) => {
                VerificationRequestState::Cancelled { cancel_info: c.into() }
            }
        }
    }

    async fn changes_listener(
        request: InnerVerificationRequest,
        mut stream: impl Stream<Item = RustVerificationRequestState> + std::marker::Unpin,
        listener: Box<dyn VerificationRequestListener>,
    ) {
        while let Some(state) = stream.next().await {
            // If we receive a done or a cancelled state we're at the end of our road, we
            // break out of the loop to deallocate the stream and finish the
            // task.
            let should_break = matches!(
                state,
                RustVerificationRequestState::Done { .. }
                    | RustVerificationRequestState::Cancelled { .. }
            );

            let state = Self::convert_verification_request(&request, state);

            listener.on_change(state);

            if should_break {
                break;
            }
        }
    }
}
