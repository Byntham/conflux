#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    #[cfg(unix)]
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod 700 {}", dir.display()))?;
    Ok(())
}

#[cfg(unix)]
pub fn sync_dir(dir: &Path) -> Result<()> {
    let f = File::open(dir).with_context(|| format!("open dir {}", dir.display()))?;
    f.sync_all().with_context(|| format!("fsync dir {}", dir.display()))?;
    Ok(())
}

#[cfg(not(unix))]
pub fn sync_dir(_dir: &Path) -> Result<()> {
    Ok(())
}

pub fn write_durable(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent")?;
    let name = path.file_name().context("path has no file name")?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp-{}", uuid::Uuid::new_v4().simple()));
    {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let mut f = opts.open(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    sync_dir(dir)?;
    Ok(())
}

pub fn ensure_private_dir_all(dir: &Path, root: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    #[cfg(unix)]
    {
        let mut current = dir;
        while current.starts_with(root) {
            fs::set_permissions(current, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("chmod 700 {}", current.display()))?;
            match current.parent() {
                Some(parent) => current = parent,
                None => break,
            }
        }
    }
    #[cfg(not(unix))]
    let _ = root;
    Ok(())
}

pub fn open_private_append(path: &Path) -> Result<fs::File> {
    let mut opts = OpenOptions::new();
    opts.append(true).create(true);
    #[cfg(unix)]
    opts.mode(0o600);
    opts.open(path).with_context(|| format!("open {}", path.display()))
}

pub fn write_json_durable<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    write_durable(path, &bytes)
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

pub fn remove_durable(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {
            if let Some(dir) = path.parent() {
                sync_dir(dir)?;
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_write_round_trip_and_mode() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.json");
        write_json_durable(&p, &serde_json::json!({"x": 1})).unwrap();
        let v: serde_json::Value = read_json(&p).unwrap().unwrap();
        assert_eq!(v["x"], 1);
        #[cfg(unix)]
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        write_json_durable(&p, &serde_json::json!({"x": 2})).unwrap();
        let v: serde_json::Value = read_json(&p).unwrap().unwrap();
        assert_eq!(v["x"], 2);
        remove_durable(&p).unwrap();
        assert!(read_json::<serde_json::Value>(&p).unwrap().is_none());
    }
}
