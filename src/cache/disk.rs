use std::any::Any;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use pingora::cache::{
    eviction::{lru::Manager, EvictionManager},
    key::{CacheHashKey, CompactCacheKey},
    storage::{HandleHit, HandleMiss, HitHandler, MissHandler, MissFinishType, PurgeType},
    CacheMeta, CacheKey, Storage,
};
use pingora::cache::trace::SpanHandle;
use pingora::{Error, ErrorType::InternalError};
use tokio::fs;
use tracing::warn;

// FILE FORMAT
// [4 LE bytes: internal_len][internal_meta bytes]
// [4 LE bytes: header_len][header_meta bytes]
// [body bytes]

pub struct DiskStore {
    dir: PathBuf,
    pub eviction: &'static Manager<16>,
}

impl DiskStore {
    pub fn new(dir: impl Into<PathBuf>, eviction: &'static Manager<16>) -> Self {
        DiskStore { dir: dir.into(), eviction }
    }

    fn object_path(&self, hash: &str) -> PathBuf {
        self.dir.join(&hash[..2]).join(format!("{hash}.keel"))
    }

    fn temp_path(&self, hash: &str) -> PathBuf {
        self.dir.join(&hash[..2]).join(format!("{hash}.keel.tmp"))
    }
}

fn encode_entry(internal: &[u8], header: &[u8], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + internal.len() + header.len() + body.len());
    out.extend_from_slice(&(internal.len() as u32).to_le_bytes());
    out.extend_from_slice(internal);
    out.extend_from_slice(&(header.len() as u32).to_le_bytes());
    out.extend_from_slice(header);
    out.extend_from_slice(body);
    out
}

fn decode_entry(data: &[u8]) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    if data.len() < 8 {
        return None;
    }
    let int_len = u32::from_le_bytes(data[..4].try_into().ok()?) as usize;
    if data.len() < 4 + int_len + 4 {
        return None;
    }
    let internal = data[4..4 + int_len].to_vec();
    let hdr_off = 4 + int_len;
    let hdr_len = u32::from_le_bytes(data[hdr_off..hdr_off + 4].try_into().ok()?) as usize;
    if data.len() < hdr_off + 4 + hdr_len {
        return None;
    }
    let header = data[hdr_off + 4..hdr_off + 4 + hdr_len].to_vec();
    let body = data[hdr_off + 4 + hdr_len..].to_vec();
    Some((internal, header, body))
}

// HIT HANDLER

struct DiskHit {
    body: Arc<Vec<u8>>,
    done: bool,
    range_start: usize,
    range_end: usize,
}

#[async_trait]
impl HandleHit for DiskHit {
    async fn read_body(&mut self) -> pingora::Result<Option<Bytes>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        Ok(Some(Bytes::copy_from_slice(&self.body[self.range_start..self.range_end])))
    }

    async fn finish(
        self: Box<Self>,
        _storage: &'static (dyn Storage + Sync),
        _key: &CacheKey,
        _trace: &SpanHandle,
    ) -> pingora::Result<()> {
        Ok(())
    }

    fn can_seek(&self) -> bool { true }

    fn seek(&mut self, start: usize, end: Option<usize>) -> pingora::Result<()> {
        if start >= self.body.len() {
            return Error::e_explain(
                InternalError,
                format!("seek {start} >= body len {}", self.body.len()),
            );
        }
        self.range_start = start;
        self.range_end = end.map_or(self.body.len(), |e| e.min(self.body.len()));
        self.done = false;
        Ok(())
    }

    fn get_eviction_weight(&self) -> usize { self.body.len() }

    fn as_any(&self) -> &(dyn Any + Send + Sync) { self }
    fn as_any_mut(&mut self) -> &mut (dyn Any + Send + Sync) { self }
}

// MISS HANDLER

pub struct DiskMiss {
    meta_internal: Vec<u8>,
    meta_header: Vec<u8>,
    body: Vec<u8>,
    key_hash: String,
    compact_key: CompactCacheKey,
    fresh_until: std::time::SystemTime,
    store: &'static DiskStore,
    finished: bool,
}

impl Drop for DiskMiss {
    fn drop(&mut self) {
        if !self.finished {
            let path = self.store.temp_path(&self.key_hash);
            let _ = std::fs::remove_file(path);
        }
    }
}

#[async_trait]
impl HandleMiss for DiskMiss {
    async fn write_body(&mut self, data: Bytes, _eof: bool) -> pingora::Result<()> {
        self.body.extend_from_slice(&data);
        Ok(())
    }

    async fn finish(mut self: Box<Self>) -> pingora::Result<MissFinishType> {
        self.finished = true;
        let hash = self.key_hash.clone();
        let shard_dir = self.store.dir.join(&hash[..2]);

        if let Err(e) = fs::create_dir_all(&shard_dir).await {
            warn!(error = %e, path = %shard_dir.display(), "disk cache: mkdir failed");
            return Ok(MissFinishType::Created(0));
        }

        let encoded = encode_entry(&self.meta_internal, &self.meta_header, &self.body);
        let size = encoded.len();
        let tmp = self.store.temp_path(&hash);
        let final_path = self.store.object_path(&hash);

        if let Err(e) = fs::write(&tmp, &encoded).await {
            warn!(error = %e, "disk cache: write failed");
            return Ok(MissFinishType::Created(0));
        }
        if let Err(e) = fs::rename(&tmp, &final_path).await {
            warn!(error = %e, "disk cache: rename failed");
            let _ = fs::remove_file(&tmp).await;
            return Ok(MissFinishType::Created(0));
        }

        // Drive eviction: remove any keys the LRU decides to evict.
        let evicted = self.store.eviction.admit(self.compact_key.clone(), size, self.fresh_until);
        for key in evicted {
            let evict_hash = key.combined();
            let _ = fs::remove_file(self.store.object_path(&evict_hash)).await;
        }

        Ok(MissFinishType::Created(size))
    }
}

// STORAGE IMPL

#[async_trait]
impl Storage for DiskStore {
    async fn lookup(
        &'static self,
        key: &CacheKey,
        _trace: &SpanHandle,
    ) -> pingora::Result<Option<(CacheMeta, HitHandler)>> {
        let hash = key.combined();
        let path = self.object_path(&hash);

        let data = match fs::read(&path).await {
            Ok(d) => d,
            Err(_) => return Ok(None),
        };

        let (internal, header, body) = match decode_entry(&data) {
            Some(t) => t,
            None => {
                warn!(path = %path.display(), "disk cache: corrupt entry, removing");
                let _ = fs::remove_file(&path).await;
                return Ok(None);
            }
        };

        let meta = match CacheMeta::deserialize(&internal, &header) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "disk cache: meta deserialize failed");
                return Ok(None);
            }
        };

        let body_len = body.len();
        let hit = DiskHit {
            body: Arc::new(body),
            done: false,
            range_start: 0,
            range_end: body_len,
        };

        let compact = key.to_compact();
        self.eviction.access(&compact, body_len, meta.fresh_until());
        Ok(Some((meta, Box::new(hit))))
    }

    async fn get_miss_handler(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        _trace: &SpanHandle,
    ) -> pingora::Result<MissHandler> {
        let (internal, header) = meta.serialize()?;
        let hash = key.combined();
        Ok(Box::new(DiskMiss {
            meta_internal: internal,
            meta_header: header,
            body: Vec::new(),
            key_hash: hash,
            compact_key: key.to_compact(),
            fresh_until: meta.fresh_until(),
            store: self,
            finished: false,
        }))
    }

    async fn purge(
        &'static self,
        key: &CompactCacheKey,
        _purge_type: PurgeType,
        _trace: &SpanHandle,
    ) -> pingora::Result<bool> {
        let hash = key.combined();
        let path = self.object_path(&hash);
        self.eviction.remove(key);
        match fs::remove_file(&path).await {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => {
                warn!(error = %e, "disk cache: purge failed");
                Ok(false)
            }
        }
    }

    async fn update_meta(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        _trace: &SpanHandle,
    ) -> pingora::Result<bool> {
        let hash = key.combined();
        let path = self.object_path(&hash);

        let data = match fs::read(&path).await {
            Ok(d) => d,
            Err(_) => return Ok(false),
        };

        let (_, _, body) = match decode_entry(&data) {
            Some(t) => t,
            None => return Ok(false),
        };

        let (new_internal, new_header) = meta.serialize()?;
        let encoded = encode_entry(&new_internal, &new_header, &body);
        let tmp = self.temp_path(&hash);

        if fs::write(&tmp, &encoded).await.is_err() {
            return Ok(false);
        }
        Ok(fs::rename(&tmp, &path).await.is_ok())
    }

    fn support_streaming_partial_write(&self) -> bool { false }

    fn as_any(&self) -> &(dyn Any + Send + Sync) { self }
}
