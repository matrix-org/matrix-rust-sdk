use std::env;

use camino::Utf8PathBuf;
use cargo_metadata::Metadata;
use serde::Deserialize;

use serde_json::Value;
use xshell::cmd;

use crate::{Result, sh};

#[derive(Deserialize, Default, Debug, Clone)]
pub struct XtaskMetadata {
    pub ffi_crate: Option<String>,
    pub kotlin_features: Option<String>,
    pub swift_features: Option<String>,
}

lazy_static::lazy_static! {
    static ref METADATA: Metadata = load_metadata().expect("failed to load `cargo metadata`");
    static ref XTASK_METADATA: XtaskMetadata = load_xtask_metadata().expect("failed to load xtask metadata from `cargo metadata`");
}

fn load_metadata() -> Result<Metadata> {
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let sh = sh();
    let metadata_json = cmd!(sh, "{cargo} metadata --no-deps --format-version 1").read()?;
    Ok(serde_json::from_str(&metadata_json)?)
}

fn load_xtask_metadata() -> Result<XtaskMetadata> {
    #[derive(Deserialize, Default)]
    struct WorkspaceMetadata {
        #[serde(default)]
        xtask: XtaskMetadata,
    }

    if METADATA.workspace_metadata == Value::Null {
        return Ok(XtaskMetadata::default());
    }

    let xtask_info =
        serde_json::from_value::<WorkspaceMetadata>(METADATA.workspace_metadata.clone())?.xtask;
    Ok(xtask_info)
}

pub fn xtask_metadata() -> Result<XtaskMetadata> {
    Ok(XTASK_METADATA.clone())
}

pub fn root_path() -> Result<Utf8PathBuf> {
    Ok(METADATA.workspace_root.clone())
}

pub fn target_path() -> Result<Utf8PathBuf> {
    Ok(METADATA.target_directory.clone())
}
