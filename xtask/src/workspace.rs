use std::env;

use camino::Utf8PathBuf;
use serde::Deserialize;
use xshell::cmd;

use crate::{Result, sh};

#[derive(Deserialize, Default, Debug)]
pub struct XtaskMetadata {
    pub ffi_crate: Option<String>,
    pub kotlin_features: Option<String>,
    pub swift_features: Option<String>,
}

pub fn xtask_metadata() -> Result<XtaskMetadata> {
    #[derive(Deserialize, Default)]
    struct WorkspaceMetadata {
        #[serde(default)]
        xtask: XtaskMetadata,
    }
    #[derive(Deserialize)]
    struct Metadata {
        #[serde(default)]
        metadata: WorkspaceMetadata,
    }

    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let sh = sh();
    let metadata_json = cmd!(sh, "{cargo} metadata --no-deps --format-version 1").read()?;
    let xtask_info = serde_json::from_str::<Metadata>(&metadata_json)?.metadata.xtask;
    Ok(xtask_info)
}

pub fn root_path() -> Result<Utf8PathBuf> {
    #[derive(Deserialize)]
    struct Metadata {
        workspace_root: Utf8PathBuf,
    }

    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let sh = sh();
    let metadata_json = cmd!(sh, "{cargo} metadata --no-deps --format-version 1").read()?;
    Ok(serde_json::from_str::<Metadata>(&metadata_json)?.workspace_root)
}

pub fn target_path() -> Result<Utf8PathBuf> {
    #[derive(Deserialize)]
    struct Metadata {
        target_directory: Utf8PathBuf,
    }

    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let sh = sh();
    let metadata_json = cmd!(sh, "{cargo} metadata --no-deps --format-version 1").read()?;
    Ok(serde_json::from_str::<Metadata>(&metadata_json)?.target_directory)
}
