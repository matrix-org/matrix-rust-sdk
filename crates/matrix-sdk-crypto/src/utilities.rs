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

use std::num::NonZeroU8;

use ruma::MilliSecondsSinceUnixEpoch;
use time::{
    OffsetDateTime,
    format_description::well_known::{Iso8601, iso8601},
};

#[cfg(test)]
pub(crate) fn json_convert<T, U>(value: &T) -> serde_json::Result<U>
where
    T: serde::Serialize,
    U: serde::de::DeserializeOwned,
{
    let json = serde_json::to_string(value)?;
    serde_json::from_str(&json)
}

const ISO8601_WITH_MILLIS: iso8601::EncodedConfig = iso8601::Config::DEFAULT
    .set_time_precision(iso8601::TimePrecision::Second { decimal_digits: NonZeroU8::new(3) })
    .encode();

/// Format the given timestamp into a human-readable timestamp.
///
/// # Returns
///
/// Provided the timestamp fits within an `OffsetDateTime` (ie, it is on or
/// before year 9999), a string that looks like `1970-01-01T00:00:00.000Z`.
/// Otherwise, `None`.
pub fn timestamp_to_iso8601(ts: MilliSecondsSinceUnixEpoch) -> Option<String> {
    let nanos_since_epoch = i128::from(ts.get()) * 1_000_000;

    // OffsetDateTime has a max year of 9999, whereas MilliSecondsSinceUnixEpoch has
    // a max year of 285427, so `from_unix_timestamp_nanos` can overflow for very
    // large timestamps. (The Y10K problem!)
    let dt = OffsetDateTime::from_unix_timestamp_nanos(nanos_since_epoch).ok()?;

    // SAFETY: `format` can fail if:
    //   * The input lacks information on a component we have asked it to format
    //     (eg, it is given a `Time` and we ask it for a date), or
    //   * The input contains an invalid component (eg 30th February), or
    //   * An `io::Error` is raised internally.
    //
    // The first two cannot occur because we know we are giving it a valid
    // OffsetDateTime that has all the components we are asking it to print.
    //
    // The third should not occur because we are formatting a short string to an
    // in-memory buffer.

    Some(dt.format(&Iso8601::<ISO8601_WITH_MILLIS>).unwrap())
}

#[cfg(test)]
pub(crate) mod tests {
    use ruma::{MilliSecondsSinceUnixEpoch, UInt};

    use super::timestamp_to_iso8601;

    #[test]
    fn test_timestamp_to_iso8601() {
        assert_eq!(
            timestamp_to_iso8601(MilliSecondsSinceUnixEpoch(UInt::new_saturating(0))),
            Some("1970-01-01T00:00:00.000Z".to_owned())
        );
        assert_eq!(
            timestamp_to_iso8601(MilliSecondsSinceUnixEpoch(UInt::new_saturating(1709657033012))),
            Some("2024-03-05T16:43:53.012Z".to_owned())
        );
        assert_eq!(timestamp_to_iso8601(MilliSecondsSinceUnixEpoch(UInt::MAX)), None);
    }
}
