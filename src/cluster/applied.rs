//! What this node serves: the config version it last applied and a hash of
//! its files, kept in the state directory. The config directory is written
//! only after a version applied, so it always holds that version — the last
//! one that worked here — and a restart compares the files against the hash
//! to see whether someone edited them while the node was stopped.

use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::FileSet;

const APPLIED_FILE: &str = "applied_config.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Applied {
    pub version: u64,
    pub hash: String,
}

/// SHA-256 over every path and content, in path order.
pub fn hash(files: &FileSet) -> String {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    for (path, text) in files {
        for part in [path.as_bytes(), text.as_bytes()] {
            ctx.update(&(part.len() as u64).to_be_bytes());
            ctx.update(part);
        }
    }
    hex::encode(ctx.finish().as_ref())
}

pub fn read(state_dir: &Path) -> Result<Option<Applied>> {
    let path = state_dir.join(APPLIED_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map(Some).with_context(|| format!("{} is not readable", path.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

pub fn save(state_dir: &Path, version: u64, files: &FileSet) -> Result<()> {
    let text = serde_json::to_string(&Applied { version, hash: hash(files) })?;
    write_atomic(state_dir, APPLIED_FILE, &text)
}

/// Make `dir` hold exactly `files`: write what is new or changed (each file
/// replaced whole, via a temporary file), delete what the set no longer has.
pub fn write_config_dir(dir: &Path, files: &FileSet) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let current = keel_control::read_file_set(dir)?;
    for (rel, text) in files {
        if current.get(rel) == Some(text) {
            continue;
        }
        let path = dir.join(rel);
        let parent = path.parent().unwrap_or(dir);
        std::fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
        let name = path.file_name().and_then(|n| n.to_str()).context("config file without a name")?;
        write_atomic(parent, name, text)?;
    }
    for rel in current.keys().filter(|rel| !files.contains_key(*rel)) {
        let path = dir.join(rel);
        std::fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
    }
    Ok(())
}

fn write_atomic(dir: &Path, name: &str, text: &str) -> Result<()> {
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, text).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, dir.join(name)).with_context(|| format!("cannot write {}", dir.join(name).display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(entries: &[(&str, &str)]) -> FileSet {
        entries.iter().map(|(p, t)| (p.to_string(), t.to_string())).collect()
    }

    #[test]
    fn the_hash_depends_on_paths_and_contents() {
        let a = set(&[("keel.yaml", "x"), ("pools/a.yaml", "y")]);
        assert_eq!(hash(&a), hash(&a.clone()));
        assert_ne!(hash(&a), hash(&set(&[("keel.yaml", "x"), ("pools/b.yaml", "y")])));
        assert_ne!(hash(&a), hash(&set(&[("keel.yaml", "xy"), ("pools/a.yaml", "")])));
    }

    #[test]
    fn the_config_directory_ends_up_holding_exactly_the_set() {
        let dir = std::env::temp_dir().join(format!("keel-applied-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_config_dir(&dir, &set(&[("keel.yaml", "v1"), ("pools/a.yaml", "a"), ("old.yaml", "gone soon")])).unwrap();
        let next = set(&[("keel.yaml", "v2"), ("pools/a.yaml", "a"), ("certs/b.crt", "pem")]);
        write_config_dir(&dir, &next).unwrap();
        assert_eq!(keel_control::read_file_set(&dir).unwrap(), next);

        save(&dir, 7, &next).unwrap();
        assert_eq!(read(&dir).unwrap(), Some(Applied { version: 7, hash: hash(&next) }));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
