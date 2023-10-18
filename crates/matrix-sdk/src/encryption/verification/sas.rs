// Copyright 2020 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use futures_core::Stream;
use matrix_sdk_base::crypto::{
    AcceptSettings, CancelInfo, Emoji, ReadOnlyDevice, Sas as BaseSas, SasState,
};
use ruma::{events::key::verification::cancel::CancelCode, UserId};

use crate::{error::Result, Client};

/// An object controlling the short auth string verification flow.
#[derive(Debug, Clone)]
pub struct SasVerification {
    pub(crate) inner: BaseSas,
    pub(crate) client: Client,
}

impl SasVerification {
    /// Accept the interactive verification flow.
    pub async fn accept(&self) -> Result<()> {
        self.accept_with_settings(Default::default()).await
    }

    /// Accept the interactive verification flow with specific settings.
    ///
    /// # Arguments
    ///
    /// * `settings` - specific customizations to the verification flow.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # use ruma::user_id;
    /// use matrix_sdk::{
    ///     encryption::verification::{AcceptSettings, SasVerification},
    ///     ruma::events::key::verification::ShortAuthenticationString,
    /// };
    ///
    /// # let flow_id = "someID";
    /// # let user_id = user_id!("@alice:example");
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// let sas = client
    ///     .encryption()
    ///     .get_verification(&user_id, flow_id)
    ///     .await
    ///     .and_then(|v| v.sas());
    ///
    /// if let Some(sas) = sas {
    ///     let only_decimal = AcceptSettings::with_allowed_methods(vec![
    ///         ShortAuthenticationString::Decimal,
    ///     ]);
    ///
    ///     sas.accept_with_settings(only_decimal).await?;
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn accept_with_settings(&self, settings: AcceptSettings) -> Result<()> {
        if let Some(request) = self.inner.accept_with_settings(settings) {
            self.client.send_verification_request(request).await?;
        }
        Ok(())
    }

    /// Confirm that the short auth strings match on both sides.
    pub async fn confirm(&self) -> Result<()> {
        let (requests, signature) = self.inner.confirm().await?;

        for request in requests {
            self.client.send_verification_request(request).await?;
        }

        if let Some(s) = signature {
            self.client.send(s, None).await?;
        }

        Ok(())
    }

    /// Cancel the interactive verification flow because the short auth strings
    /// didn't match on both sides.
    pub async fn mismatch(&self) -> Result<()> {
        if let Some(request) = self.inner.cancel_with_code(CancelCode::MismatchedSas) {
            self.client.send_verification_request(request).await?;
        }

        Ok(())
    }

    /// Cancel the interactive verification flow.
    pub async fn cancel(&self) -> Result<()> {
        if let Some(request) = self.inner.cancel() {
            self.client.send_verification_request(request).await?;
        }

        Ok(())
    }

    /// Get the emoji version of the short auth string.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # use ruma::user_id;
    /// use matrix_sdk::{
    ///     encryption::verification::{AcceptSettings, SasVerification},
    ///     ruma::events::key::verification::ShortAuthenticationString,
    /// };
    ///
    /// # let flow_id = "someID";
    /// # let user_id = user_id!("@alice:example");
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// let sas_verification = client
    ///     .encryption()
    ///     .get_verification(&user_id, flow_id)
    ///     .await
    ///     .and_then(|v| v.sas());
    ///
    /// if let Some(emojis) = sas_verification.and_then(|s| s.emoji()) {
    ///     let emoji_string = emojis
    ///         .iter()
    ///         .map(|e| format!("{:^12}", e.symbol))
    ///         .collect::<Vec<_>>()
    ///         .join("");
    ///
    ///     let description = emojis
    ///         .iter()
    ///         .map(|e| format!("{:^12}", e.description))
    ///         .collect::<Vec<_>>()
    ///         .join("");
    ///
    ///     println!("Do the emojis match?\n{emoji_string}\n{description}");
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub fn emoji(&self) -> Option<[Emoji; 7]> {
        self.inner.emoji()
    }

    /// Get the decimal version of the short auth string.
    pub fn decimals(&self) -> Option<(u16, u16, u16)> {
        self.inner.decimals()
    }

    /// Does this verification flow support emoji for the short authentication
    /// string.
    pub fn supports_emoji(&self) -> bool {
        self.inner.supports_emoji()
    }

    /// Is the verification process done.
    pub fn is_done(&self) -> bool {
        self.inner.is_done()
    }

    /// Are we in a state where we can show the short auth string.
    pub fn can_be_presented(&self) -> bool {
        self.inner.can_be_presented()
    }

    /// Did we initiate the verification flow.
    pub fn we_started(&self) -> bool {
        self.inner.we_started()
    }

    /// Get info about the cancellation if the verification flow has been
    /// cancelled.
    pub fn cancel_info(&self) -> Option<CancelInfo> {
        self.inner.cancel_info()
    }

    /// Is the verification process canceled.
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    /// Get the other users device that we're verifying.
    pub fn other_device(&self) -> &ReadOnlyDevice {
        self.inner.other_device()
    }

    /// Did this verification flow start from a verification request.
    pub fn started_from_request(&self) -> bool {
        self.inner.started_from_request()
    }

    /// Is this a verification that is verifying one of our own devices.
    pub fn is_self_verification(&self) -> bool {
        self.inner.is_self_verification()
    }

    /// Get our own user id.
    pub fn own_user_id(&self) -> &UserId {
        self.inner.user_id()
    }

    /// Get the user id of the other user participating in this verification
    /// flow.
    pub fn other_user_id(&self) -> &UserId {
        self.inner.other_user_id()
    }

    /// Listen for changes in the SAS verification process.
    ///
    /// The changes are presented as a stream of [`SasState`] values.
    ///
    /// This method can be used to react to changes in the state of the
    /// verification process, or rather the method can be used to handle
    /// each step of the verification process.
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
    /// # Examples
    ///
    /// ```no_run
    /// use futures_util::{Stream, StreamExt};
    /// use matrix_sdk::encryption::verification::{SasState, SasVerification};
    ///
    /// # async {
    /// # let sas: SasVerification = unimplemented!();
    /// # let user_confirmed = false;
    /// let mut stream = sas.changes();
    ///
    /// while let Some(state) = stream.next().await {
    ///     match state {
    ///         SasState::KeysExchanged { emojis, decimals: _ } => {
    ///             let emojis =
    ///                 emojis.expect("We only support emoji verification");
    ///             println!("Do these emojis match {emojis:#?}");
    ///
    ///             // Ask the user to confirm or cancel here.
    ///             if user_confirmed {
    ///                 sas.confirm().await?;
    ///             } else {
    ///                 sas.cancel().await?;
    ///             }
    ///         }
    ///         SasState::Done { .. } => {
    ///             let device = sas.other_device();
    ///
    ///             println!(
    ///                 "Successfully verified device {} {} {:?}",
    ///                 device.user_id(),
    ///                 device.device_id(),
    ///                 device.local_trust_state()
    ///             );
    ///
    ///             break;
    ///         }
    ///         SasState::Cancelled(cancel_info) => {
    ///             println!(
    ///                 "The verification has been cancelled, reason: {}",
    ///                 cancel_info.reason()
    ///             );
    ///             break;
    ///         }
    ///         SasState::Started { .. }
    ///         | SasState::Accepted { .. }
    ///         | SasState::Confirmed => (),
    ///     }
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub fn changes(&self) -> impl Stream<Item = SasState> {
        self.inner.changes()
    }

    /// Get the current state the verification process is in.
    ///
    /// To listen to changes to the [`SasState`] use the
    /// [`SasVerification::changes`] method.
    pub fn state(&self) -> SasState {
        self.inner.state()
    }
}
