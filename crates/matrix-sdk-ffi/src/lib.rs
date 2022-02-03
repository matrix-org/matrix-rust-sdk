// TODO: target-os conditional would be good.
mod ios;
use ios::*;

uniffi_macros::include_scaffolding!("api");