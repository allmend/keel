use std::any::Any;

use async_trait::async_trait;
use bytes::Bytes;
use pingora::cache::{
    storage::{HandleMiss, HitHandler, MissHandler, MissFinishType, PurgeOutcome, PurgeTarget, PurgeType},
    CacheMeta, CacheKey, MemCache, Storage,
};
use pingora::cache::trace::SpanHandle;

use super::disk::DiskStore;

/// L1 (memory) + L2 (disk) tiered cache.
///
/// `lookup` checks L1 first, then falls back to L2.
/// `get_miss_handler` writes to both tiers simultaneously so every cached
/// response ends up in both after the first miss.
pub struct TieredStore {
    pub l1: &'static MemCache,
    pub l2: &'static DiskStore,
}

#[async_trait]
impl Storage for TieredStore {
    async fn lookup(
        &'static self,
        key: &CacheKey,
        trace: &SpanHandle,
    ) -> pingora::Result<Option<(CacheMeta, HitHandler)>> {
        // L1 fast path
        if let Some(hit) = self.l1.lookup(key, trace).await? {
            return Ok(Some(hit));
        }
        // L2 fallback
        self.l2.lookup(key, trace).await
    }

    async fn get_miss_handler(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &SpanHandle,
    ) -> pingora::Result<MissHandler> {
        let l1 = self.l1.get_miss_handler(key, meta, trace).await?;
        let l2 = self.l2.get_miss_handler(key, meta, trace).await?;
        Ok(Box::new(TieredMissHandler { l1: Some(l1), l2: Some(l2) }))
    }

    async fn purge(
        &'static self,
        target: PurgeTarget<'_>,
        purge_type: PurgeType,
        trace: &SpanHandle,
    ) -> pingora::Result<PurgeOutcome> {
        // Both tiers are asked, and the weakest outcome wins: if either still
        // holds the entry as stale, the eviction manager must keep tracking it.
        let r1 = self.l1.purge(target, purge_type, trace).await?;
        let r2 = self.l2.purge(target, purge_type, trace).await?;
        Ok(combine(r1, r2))
    }

    async fn expire(
        &'static self,
        target: PurgeTarget<'_>,
        trace: &SpanHandle,
    ) -> pingora::Result<PurgeOutcome> {
        // Without this the default would delete from both tiers, losing the
        // body a revalidation could have reused.
        let r1 = self.l1.expire(target, trace).await?;
        let r2 = self.l2.expire(target, trace).await?;
        Ok(combine(r1, r2))
    }

    async fn update_meta(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &SpanHandle,
    ) -> pingora::Result<bool> {
        let r1 = self.l1.update_meta(key, meta, trace).await?;
        let r2 = self.l2.update_meta(key, meta, trace).await?;
        Ok(r1 || r2)
    }

    fn support_streaming_partial_write(&self) -> bool { false }

    fn as_any(&self) -> &(dyn Any + Send + Sync) { self }
}

/// One outcome for two tiers. `Expired` outranks `Purged` because the entry
/// still exists somewhere, and the eviction manager stops tracking what it is
/// told was purged.
fn combine(a: PurgeOutcome, b: PurgeOutcome) -> PurgeOutcome {
    match (a, b) {
        (PurgeOutcome::NotFound, PurgeOutcome::NotFound) => PurgeOutcome::NotFound,
        (PurgeOutcome::Expired, _) | (_, PurgeOutcome::Expired) => PurgeOutcome::Expired,
        _ => PurgeOutcome::Purged(None),
    }
}

struct TieredMissHandler {
    l1: Option<MissHandler>,
    l2: Option<MissHandler>,
}

#[async_trait]
impl HandleMiss for TieredMissHandler {
    async fn write_body(&mut self, data: Bytes, eof: bool) -> pingora::Result<()> {
        if let Some(h) = self.l1.as_mut() {
            h.write_body(data.clone(), eof).await?;
        }
        if let Some(h) = self.l2.as_mut() {
            h.write_body(data, eof).await?;
        }
        Ok(())
    }

    async fn finish(mut self: Box<Self>) -> pingora::Result<MissFinishType> {
        let l1_result = match self.l1.take() {
            Some(h) => h.finish().await?,
            None => MissFinishType::Created(0),
        };
        // L2 writes independently; if it fails we still served from L1.
        if let Some(h) = self.l2.take() {
            let _ = h.finish().await;
        }
        Ok(l1_result)
    }
}
