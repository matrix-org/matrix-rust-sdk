use instant::SystemTime;
use ruma::MilliSecondsSinceUnixEpoch;

/// Platform agnostic helper function to create MilliSecondsSinceUnixEpoch
pub fn milli_seconds_since_unix_epoch() -> MilliSecondsSinceUnixEpoch {
    let duration =
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).expect("now is always higher");
    let millis =
        duration.as_millis().try_into().expect("can't convert miliseconds since UNIXEPOCH");
    MilliSecondsSinceUnixEpoch(millis)
}
