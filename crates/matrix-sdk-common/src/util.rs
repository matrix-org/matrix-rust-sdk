use instant::{Duration, SystemTime};
use ruma::{MilliSecondsSinceUnixEpoch, SecondsSinceUnixEpoch};

/// Platform agnostic helper function to create MilliSecondsSinceUnixEpoch
pub fn milli_seconds_since_unix_epoch() -> MilliSecondsSinceUnixEpoch {
    let duration =
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).expect("now is always higher");
    let millis =
        duration.as_millis().try_into().expect("can't convert milliseconds since UNIXEPOCH");
    MilliSecondsSinceUnixEpoch(millis)
}

/// Platform agnostic helper function to create SecondsSinceUnixEpoch
pub fn seconds_since_unix_epoch() -> SecondsSinceUnixEpoch {
    modified_seconds_since_unix_epoch(|e| e)
}

/// Platform agnostic helper function to create SecondsSinceUnixEpoch with
/// modifications
pub fn modified_seconds_since_unix_epoch<F: Fn(Duration) -> Duration>(
    f: F,
) -> SecondsSinceUnixEpoch {
    let duration =
        f(SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).expect("now is always higher"));

    let millis = duration.as_secs().try_into().expect("can't convert seconds since UNIXEPOCH");
    SecondsSinceUnixEpoch(millis)
}
