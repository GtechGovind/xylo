//! Shared ownership hints for the host's managed local provider. This registry
//! only constrains concurrency; it is not proof of health or authorization.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const MAX_BYTES: u64 = 256 * 1024;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Entry {
    single_slot: bool,
    pid: u32,
}

pub(crate) struct Registration {
    data: PathBuf,
    endpoint: String,
    pid: u32,
}

impl Registration {
    pub(crate) fn create(data: &Path, endpoint: &str, pid: u32) -> Result<Self> {
        let url = url::Url::parse(endpoint)?;
        ensure!(
            url.scheme() == "http" && url.host_str() == Some("127.0.0.1") && url.port().is_some(),
            "Managed endpoint must be explicit loopback HTTP"
        );
        ensure!(pid > 0, "Managed endpoint PID must be nonzero");
        let endpoint = url.to_string().trim_end_matches('/').to_owned();
        mutate(data, |entries| {
            entries.insert(
                endpoint.clone(),
                Entry {
                    single_slot: true,
                    pid,
                },
            );
        })?;
        Ok(Self {
            data: data.to_path_buf(),
            endpoint,
            pid,
        })
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let result = mutate(&self.data, |entries| {
            // Do not remove a replacement owned by a newer server.
            if entries
                .get(&self.endpoint)
                .is_some_and(|entry| entry.pid == self.pid)
            {
                entries.remove(&self.endpoint);
            }
        });
        if let Err(error) = result {
            tracing::warn!(%error,"Could not remove managed endpoint ownership marker");
        }
    }
}

fn mutate(data: &Path, update: impl FnOnce(&mut BTreeMap<String, Entry>)) -> Result<()> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(data.join("managed-endpoints.lock"))?;
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        match fs2::FileExt::try_lock_exclusive(&lock) {
            Ok(()) => break,
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                ensure!(
                    Instant::now() < deadline,
                    "Managed endpoint registry is busy"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error.into()),
        }
    }
    let target = data.join("managed-endpoints.json");
    let mut entries = if target.exists() {
        let file = File::open(&target)?;
        ensure!(
            file.metadata()?.len() <= MAX_BYTES,
            "Managed endpoint registry exceeds budget"
        );
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "Managed endpoint registry exceeds budget"
        );
        serde_json::from_slice::<BTreeMap<String, Entry>>(&bytes)
            .context("Invalid managed endpoint registry")?
    } else {
        BTreeMap::new()
    };
    update(&mut entries);
    ensure!(
        entries.len() <= 256,
        "Managed endpoint registry exceeds count limit"
    );
    let bytes = serde_json::to_vec(&entries)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "Managed endpoint registry exceeds budget"
    );
    let temporary = data.join(format!("managed-endpoints-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &target)?;
        #[cfg(unix)]
        File::open(data)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ownership_is_normalized_and_cleanup_preserves_other_servers() {
        let root = tempfile::tempdir().unwrap();
        let first = Registration::create(root.path(), "http://127.0.0.1:12121/v1/", 42).unwrap();
        let second = Registration::create(root.path(), "http://127.0.0.1:12122/v1", 43).unwrap();
        let read = || {
            serde_json::from_slice::<serde_json::Value>(
                &std::fs::read(root.path().join("managed-endpoints.json")).unwrap(),
            )
            .unwrap()
        };
        assert_eq!(read()["http://127.0.0.1:12121/v1"]["single_slot"], true);
        drop(first);
        assert!(read().get("http://127.0.0.1:12121/v1").is_none());
        assert_eq!(read()["http://127.0.0.1:12122/v1"]["pid"], 43);
        drop(second);
        assert_eq!(read(), serde_json::json!({}));
    }
}
