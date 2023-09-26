// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use std::{fmt, fmt::Write};

use tracing::{
    field::{Field, Visit},
    metadata::LevelFilter,
    Event, Level, Metadata, Subscriber,
};
use tracing_subscriber::layer::Context;

///! Utilities for `tracing` in wasm environments

/// An implementation of `tracing_subscriber::layer::Layer` which directs all
/// events to the JS console
#[derive(Debug)]
pub struct Layer {
    min_level: Level,
    enabled: bool,
}

impl Layer {
    pub fn new<L>(min_level: L) -> Self
    where
        L: Into<Level>,
    {
        Self { min_level: min_level.into(), enabled: true }
    }

    pub fn turn_on(&mut self) {
        self.enabled = true;
    }

    pub fn turn_off(&mut self) {
        self.enabled = false;
    }

    pub fn set_min_level(&mut self, min_level: Level) {
        self.min_level = min_level;
    }
}

impl<S> tracing_subscriber::layer::Layer<S> for Layer
where
    S: Subscriber,
{
    fn enabled(&self, metadata: &Metadata<'_>, _: Context<'_, S>) -> bool {
        self.enabled && metadata.level() <= &self.min_level
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        if !self.enabled {
            Some(LevelFilter::OFF)
        } else {
            Some(LevelFilter::from_level(self.min_level))
        }
    }

    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut recorder = StringVisitor::new();
        event.record(&mut recorder);
        let metadata = event.metadata();
        let level = metadata.level();

        let origin = metadata
            .file()
            .and_then(|file| metadata.line().map(|ln| format!("{file}:{ln}")))
            .unwrap_or_default();

        let message = format!("{level} {origin}{recorder}");

        match *level {
            Level::TRACE | Level::DEBUG => web_sys::console::debug_1(&message.into()),
            Level::INFO => web_sys::console::info_1(&message.into()),
            Level::WARN => web_sys::console::warn_1(&message.into()),
            Level::ERROR => web_sys::console::error_1(&message.into()),
        }
    }
}

struct StringVisitor {
    string: String,
}

impl StringVisitor {
    fn new() -> Self {
        Self { string: String::new() }
    }
}

impl Visit for StringVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        match field.name() {
            "message" => {
                if !self.string.is_empty() {
                    self.string.push('\n');
                }

                let _ = write!(self.string, "{value:?}");
            }

            field_name => {
                let _ = write!(self.string, "\n{field_name} = {value:?}");
            }
        }
    }
}

impl fmt::Display for StringVisitor {
    fn fmt(&self, mut f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.string.is_empty() {
            write!(&mut f, " {}", self.string)
        } else {
            Ok(())
        }
    }
}
