use crate::{read_json, write_json};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub struct Run {
    pub directory: PathBuf,
    _lock: File,
}

#[derive(Serialize, Deserialize)]
pub struct Event {
    pub time: u64,
    pub phase: String,
    pub status: String,
    pub detail: serde_json::Value,
}

impl Run {
    pub fn open(directory: &Path, plan_digest: &str) -> Result<Self> {
        if !directory.exists() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new()
                    .mode(0o700)
                    .recursive(true)
                    .create(directory)?;
            }
            #[cfg(not(unix))]
            anyhow::bail!("run directories currently require Unix permissions");
        }
        let meta = fs::symlink_metadata(directory)?;
        ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "run directory must be a real directory"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                meta.permissions().mode() & 0o077 == 0,
                "run directory must have mode 0700"
            );
        }
        let lock_path = directory.join("lock");
        let mut opts = OpenOptions::new();
        opts.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        if let Ok(m) = fs::symlink_metadata(&lock_path) {
            ensure!(
                m.is_file() && !m.file_type().is_symlink(),
                "invalid run lock"
            );
        }
        let lock = opts.open(lock_path)?;
        lock.try_lock().context("another process owns this run")?;
        let binding = directory.join("binding.json");
        if binding.exists() {
            let saved: String = read_json(&binding)?;
            ensure!(saved == plan_digest, "run belongs to a different plan");
        } else {
            write_json(&binding, &plan_digest)?;
        }
        Ok(Self {
            directory: directory.into(),
            _lock: lock,
        })
    }

    pub fn event(&self, phase: &str, status: &str, detail: serde_json::Value) -> Result<()> {
        let path = self.directory.join("journal.jsonl");
        if let Ok(m) = fs::symlink_metadata(&path) {
            ensure!(
                m.is_file() && !m.file_type().is_symlink(),
                "invalid journal"
            );
        }
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(path)?;
        let event = Event {
            time: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            phase: phase.into(),
            status: status.into(),
            detail,
        };
        serde_json::to_writer(&mut file, &event)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        Ok(())
    }

    pub fn events(&self) -> Result<Vec<Event>> {
        let path = self.directory.join("journal.jsonl");
        if !path.exists() {
            return Ok(vec![]);
        }
        fs::read_to_string(path)?
            .lines()
            .map(|line| {
                serde_json::from_str(line)
                    .context("invalid or interrupted journal record; inspect before resuming")
            })
            .collect()
    }
}
