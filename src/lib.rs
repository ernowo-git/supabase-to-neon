pub mod adaptation;
pub mod auth;
pub mod config;
pub mod database;
pub mod plan;
pub mod references;
pub mod run;
pub mod storage;

use anyhow::{Context, Result, ensure};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{fs, io::Write, path::Path};

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(&fs::read(path).context("cannot read JSON artifact")?)
        .context("invalid JSON artifact")
}

// New artifacts never replace existing files or follow destination symlinks.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ensure!(parent.is_dir(), "artifact parent directory does not exist");
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path)
        .context("artifact already exists or cannot be written")?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    write_private(path, &serde_json::to_vec_pretty(value)?)
}
