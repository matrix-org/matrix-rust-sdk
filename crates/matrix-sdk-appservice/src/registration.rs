// Copyright 2022 Famedly GmbH
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

//! AppService Registration.

use std::{fs::File, ops::Deref, path::PathBuf};

use http::Uri;
use regex::Regex;
use ruma::api::appservice::Registration;

use crate::{Error, Result};

pub type Host = String;
pub type Port = u16;

/// AppService Registration
///
/// Wrapper around [`Registration`]. See also <https://matrix.org/docs/spec/application_service/r0.1.2#registration>.
#[derive(Debug, Clone)]
pub struct AppServiceRegistration {
    inner: Registration,
}

impl AppServiceRegistration {
    /// Try to load registration from yaml string
    ///
    /// See the fields of [`Registration`] for the required format
    pub fn try_from_yaml_str(value: impl AsRef<str>) -> Result<Self> {
        Ok(Self { inner: serde_yaml::from_str(value.as_ref())? })
    }

    /// Try to load registration from yaml file
    ///
    /// See the fields of [`Registration`] for the required format
    pub fn try_from_yaml_file(path: impl Into<PathBuf>) -> Result<Self> {
        let file = File::open(path.into())?;

        Ok(Self { inner: serde_yaml::from_reader(file)? })
    }

    /// Get the host and port from the registration URL
    ///
    /// If no port is found it falls back to scheme defaults: 80 for http and
    /// 443 for https
    pub fn get_host_and_port(&self) -> Result<(Host, Port)> {
        let uri = Uri::try_from(&self.inner.url)?;

        let host = uri.host().ok_or(Error::MissingRegistrationHost)?.to_owned();
        let port = match uri.port() {
            Some(port) => Ok(port.as_u16()),
            None => match uri.scheme_str() {
                Some("http") => Ok(80),
                Some("https") => Ok(443),
                _ => Err(Error::MissingRegistrationPort),
            },
        }?;

        Ok((host, port))
    }
}

impl From<Registration> for AppServiceRegistration {
    fn from(value: Registration) -> Self {
        Self { inner: value }
    }
}

impl Deref for AppServiceRegistration {
    type Target = Registration;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// Cache data for the registration namespaces.
#[derive(Debug, Clone)]
pub struct NamespaceCache {
    /// List of user regexes in our namespace
    pub(crate) users: Vec<Regex>,
    /// List of alias regexes in our namespace
    #[allow(dead_code)]
    aliases: Vec<Regex>,
    /// List of room id regexes in our namespace
    #[allow(dead_code)]
    rooms: Vec<Regex>,
}

impl NamespaceCache {
    /// Creates a new registration cache from a [`Registration`] value
    pub fn from_registration(registration: &Registration) -> Result<Self> {
        let users = registration
            .namespaces
            .users
            .iter()
            .map(|user| Regex::new(&user.regex))
            .collect::<Result<Vec<_>, _>>()?;
        let aliases = registration
            .namespaces
            .aliases
            .iter()
            .map(|user| Regex::new(&user.regex))
            .collect::<Result<Vec<_>, _>>()?;
        let rooms = registration
            .namespaces
            .rooms
            .iter()
            .map(|user| Regex::new(&user.regex))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(NamespaceCache { users, aliases, rooms })
    }
}
