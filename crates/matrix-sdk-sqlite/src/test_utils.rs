#[cfg(target_family = "wasm")]
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

#[cfg(not(target_family = "wasm"))]
use tempfile::{TempDir, tempdir};

#[cfg(not(target_family = "wasm"))]
pub const fn create_tmp_dir() -> LazyLock<TempDirWrapper> {
    LazyLock::new(|| tempdir().unwrap())
}

#[cfg(target_family = "wasm")]
pub const fn create_tmp_dir() -> LazyLock<TempDirWrapper> {
    LazyLock::new(|| TempDirWrapper::new())
}

#[cfg(not(target_family = "wasm"))]
pub type TempDirWrapper = TempDir;
#[cfg(target_family = "wasm")]
/// Wrapper type to keep interface compatibility with `TempDir`
/// for wasm environments.
pub struct TempDirWrapper(PathBuf);

#[cfg(target_family = "wasm")]
impl TempDirWrapper {
    pub fn new() -> Self {
        Self(PathBuf::from(uuid::Uuid::new_v4().to_string()))
    }

    pub fn new_with_path(path: PathBuf) -> Self {
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}
