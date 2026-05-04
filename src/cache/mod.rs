pub mod disk;
pub mod tiered;

use anyhow::{bail, Result};
use pingora::cache::{eviction::lru::Manager, MemCache, Storage};

use crate::config::CacheConfig;
use disk::DiskStore;
use tiered::TieredStore;

/// Returned by [init] — holds `'static` pointers required by Pingora.
pub struct CacheHandle {
    pub storage: &'static (dyn Storage + Sync),
    pub eviction: &'static Manager<16>,
}

/// Initialise cache storage from config.
///
/// Returns `None` if neither `memory` nor `disk` is configured.
pub fn init(cfg: &CacheConfig) -> Result<Option<CacheHandle>> {
    let mem_bytes = cfg.memory.as_deref().map(parse_size).transpose()?;
    let disk_cfg = cfg.disk.as_ref();

    match (mem_bytes, disk_cfg) {
        (None, None) => Ok(None),

        (Some(mem), None) => {
            let storage = Box::leak(Box::new(MemCache::new()));
            let capacity = (mem / (16 * 1024)).max(256);
            let eviction = Box::leak(Box::new(Manager::<16>::with_capacity(mem, capacity)));
            Ok(Some(CacheHandle { storage, eviction }))
        }

        (None, Some(disk)) => {
            let disk_bytes = parse_size(&disk.size)?;
            let capacity = (disk_bytes / (64 * 1024)).max(256);
            let eviction: &'static Manager<16> =
                Box::leak(Box::new(Manager::<16>::with_capacity(disk_bytes, capacity)));
            let store: &'static DiskStore =
                Box::leak(Box::new(DiskStore::new(&disk.path, eviction)));
            Ok(Some(CacheHandle { storage: store, eviction }))
        }

        (Some(mem), Some(disk)) => {
            let disk_bytes = parse_size(&disk.size)?;

            // Memory eviction tracks what's hot in L1.
            let mem_capacity = (mem / (16 * 1024)).max(256);
            let mem_eviction: &'static Manager<16> =
                Box::leak(Box::new(Manager::<16>::with_capacity(mem, mem_capacity)));
            let l1: &'static MemCache = Box::leak(Box::new(MemCache::new()));

            // Disk eviction is tracked separately by DiskStore internally.
            let disk_capacity = (disk_bytes / (64 * 1024)).max(256);
            let disk_eviction: &'static Manager<16> =
                Box::leak(Box::new(Manager::<16>::with_capacity(disk_bytes, disk_capacity)));
            let l2: &'static DiskStore =
                Box::leak(Box::new(DiskStore::new(&disk.path, disk_eviction)));

            let tiered: &'static TieredStore = Box::leak(Box::new(TieredStore { l1, l2 }));
            Ok(Some(CacheHandle { storage: tiered, eviction: mem_eviction }))
        }
    }
}

/// Parse a human-readable size string into bytes.
///
/// Accepts "256M", "1G", "512K", "1024" (bare number = bytes).
/// Units are case-insensitive. IEC binary prefixes (1 K = 1024).
pub fn parse_size(s: &str) -> Result<usize> {
    let s = s.trim();
    let (digits, suffix) = s
        .find(|c: char| c.is_alphabetic())
        .map(|i| s.split_at(i))
        .unwrap_or((s, ""));

    let n: usize = digits
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid size '{s}'"))?;

    let multiplier = match suffix.trim().to_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" => 1024,
        "M" | "MB" => 1024 * 1024,
        "G" | "GB" => 1024 * 1024 * 1024,
        other => bail!("unknown size unit '{other}' in '{s}'"),
    };

    Ok(n * multiplier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sizes() {
        assert_eq!(parse_size("256M").unwrap(), 256 * 1024 * 1024);
        assert_eq!(parse_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("512K").unwrap(), 512 * 1024);
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("256MB").unwrap(), 256 * 1024 * 1024);
        assert!(parse_size("bad").is_err());
        assert!(parse_size("10X").is_err());
    }
}
