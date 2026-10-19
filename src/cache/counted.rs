//! A cache tier that counts its hits for `keel_cache_hits_total{tier}`.

use std::any::Any;

use async_trait::async_trait;
use pingora::cache::storage::{HitHandler, MissHandler, PurgeOutcome, PurgeTarget, PurgeType};
use pingora::cache::trace::SpanHandle;
use pingora::cache::{CacheKey, CacheMeta, Storage};

pub struct Counted<S: 'static> {
    pub inner: &'static S,
    /// `memory` or `disk`.
    pub tier: &'static str,
}

#[async_trait]
impl<S: Storage + Sync + Send + 'static> Storage for Counted<S> {
    async fn lookup(&'static self, key: &CacheKey, trace: &SpanHandle) -> pingora::Result<Option<(CacheMeta, HitHandler)>> {
        let hit = self.inner.lookup(key, trace).await?;
        if hit.is_some() {
            crate::metrics::record_cache_hit(self.tier);
        }
        Ok(hit)
    }

    async fn get_miss_handler(&'static self, key: &CacheKey, meta: &CacheMeta, trace: &SpanHandle) -> pingora::Result<MissHandler> {
        self.inner.get_miss_handler(key, meta, trace).await
    }

    async fn purge(&'static self, target: PurgeTarget<'_>, purge_type: PurgeType, trace: &SpanHandle) -> pingora::Result<PurgeOutcome> {
        self.inner.purge(target, purge_type, trace).await
    }

    async fn expire(&'static self, target: PurgeTarget<'_>, trace: &SpanHandle) -> pingora::Result<PurgeOutcome> {
        self.inner.expire(target, trace).await
    }

    async fn update_meta(&'static self, key: &CacheKey, meta: &CacheMeta, trace: &SpanHandle) -> pingora::Result<bool> {
        self.inner.update_meta(key, meta, trace).await
    }

    fn support_streaming_partial_write(&self) -> bool {
        self.inner.support_streaming_partial_write()
    }

    // Hit handlers may downcast the storage they are finished with to their
    // own type.
    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self.inner.as_any()
    }
}
