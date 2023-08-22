// Copyright 2022 Kévin Commaille
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

use serde::{de, ser::SerializeStruct, Deserialize, Serialize};

use super::SessionTokens;

impl Serialize for SessionTokens {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let Self { access_token, refresh_token, latest_id_token } = self;

        let len = 1 + refresh_token.is_some() as usize + latest_id_token.is_some() as usize;
        let mut st = serializer.serialize_struct("SessionTokens", len)?;

        st.serialize_field("access_token", access_token)?;

        if let Some(refresh_token) = refresh_token {
            st.serialize_field("refresh_token", refresh_token)?;
        }

        if let Some(latest_id_token) = latest_id_token {
            st.serialize_field("latest_id_token", latest_id_token.as_str())?;
        }

        st.end()
    }
}

impl<'de> Deserialize<'de> for SessionTokens {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let SessionTokensDeHelper { access_token, refresh_token, latest_id_token } =
            SessionTokensDeHelper::deserialize(deserializer)?;

        let latest_id_token =
            latest_id_token.map(|s| s.try_into().map_err(de::Error::custom)).transpose()?;

        Ok(Self { access_token, refresh_token, latest_id_token })
    }
}

#[derive(Deserialize)]
struct SessionTokensDeHelper {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub latest_id_token: Option<String>,
}
