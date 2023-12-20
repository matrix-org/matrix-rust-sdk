use std::env;

use camino::Utf8PathBuf;
use serde::Deserialize;
use xshell::cmd;

use crate::Result;

pub fn root_path() -> Result<Utf8PathBuf> {
    #[derive(Deserialize)]
    struct Metadata {
        workspace_root: Utf8PathBuf,
    }

    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let metadata_json = cmd!("{cargo} metadata --no-deps --format-version 1").read()?;
    Ok(serde_json::from_str::<Metadata>(&metadata_json)?.workspace_root)
}

pub fn target_path() -> Result<Utf8PathBuf> {
    #[derive(Deserialize)]
    struct Metadata {
        target_directory: Utf8PathBuf,
    }

    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let metadata_json = cmd!("{cargo} metadata --no-deps --format-version 1").read()?;
    Ok(serde_json::from_str::<Metadata>(&metadata_json)?.target_directory)
}
