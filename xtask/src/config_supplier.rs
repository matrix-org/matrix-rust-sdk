use std::{collections::HashMap, fs};

use anyhow::{bail, Context};
use camino::Utf8PathBuf;
use cargo_metadata::Metadata;
use toml::value::Table;
use uniffi_bindgen::BindgenCrateConfigSupplier;

/// An implementation of `BindgenCrateConfigSupplier` that uses the
/// `uniffi.toml` files found in the crates of a Cargo workspace.
#[derive(Debug, Clone, Default)]
pub struct CrateConfigSupplier {
    paths: HashMap<String, Utf8PathBuf>,
}

fn load_toml_file(path: Option<&Utf8PathBuf>) -> anyhow::Result<Option<Table>> {
    if let Some(path) = path {
        if path.exists() {
            let contents = fs::read_to_string(path)?;
            let table: Table = toml::from_str(&contents)?;
            Ok(Some(table))
        } else {
            Ok(None)
        }
    } else {
        Ok(None)
    }
}

impl BindgenCrateConfigSupplier for CrateConfigSupplier {
    fn get_toml(&self, crate_name: &str) -> anyhow::Result<Option<Table>> {
        load_toml_file(self.get_toml_path(crate_name).as_ref())
    }

    fn get_toml_path(&self, crate_name: &str) -> Option<Utf8PathBuf> {
        self.paths.get(crate_name).map(|p| p.join("uniffi.toml"))
    }

    fn get_udl(&self, crate_name: &str, udl_name: &str) -> anyhow::Result<String> {
        let path = self
            .paths
            .get(crate_name)
            .context(format!("No path known to UDL files for '{crate_name}'"))?
            .join("src")
            .join(format!("{udl_name}.udl"));
        if path.exists() {
            Ok(fs::read_to_string(path)?)
        } else {
            bail!(format!("No UDL file found at '{path}'"));
        }
    }
}

impl From<Metadata> for CrateConfigSupplier {
    fn from(metadata: Metadata) -> Self {
        let paths: HashMap<String, Utf8PathBuf> = metadata
            .packages
            .iter()
            .flat_map(|p| {
                p.targets
                    .iter()
                    .filter(|t| {
                        !t.is_bin()
                            && !t.is_example()
                            && !t.is_test()
                            && !t.is_bench()
                            && !t.is_custom_build()
                    })
                    .filter_map(|t| {
                        p.manifest_path.parent().map(|p| (t.name.replace('-', "_"), p.to_owned()))
                    })
            })
            .collect();
        Self { paths }
    }
}
