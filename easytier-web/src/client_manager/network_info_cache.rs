//! Shares device network-info collection between everything that views it.
//!
//! Every dashboard tab, and every device of a user that has one open, polls
//! the same machines. Concurrent requests for one machine wait for a single
//! device RPC, and a result is reused for a short time, so the device load no
//! longer grows with the number of viewers.

use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use dashmap::DashMap;
use easytier::proto::api::manage::CollectNetworkInfoResponse;

use crate::db::UserIdInDb;

/// Shorter than the dashboard's two-second refresh, so each refresh still
/// observes a new collection.
const FRESH_FOR: Duration = Duration::from_millis(1500);
/// Entries of machines nobody has asked about for this long are dropped.
const IDLE_FOR: Duration = Duration::from_secs(60);

type Slot = Arc<tokio::sync::Mutex<Option<(Instant, CollectNetworkInfoResponse)>>>;

#[derive(Debug, Default)]
pub struct NetworkInfoCache {
    slots: DashMap<(UserIdInDb, uuid::Uuid), Slot>,
}

impl NetworkInfoCache {
    /// The machine's full network info: a result collected within
    /// [`FRESH_FOR`], or the result of `collect`, which runs at most once at a
    /// time per machine. Failures are not cached.
    pub async fn get_or_collect<E, F, Fut>(
        &self,
        user_id: UserIdInDb,
        machine_id: uuid::Uuid,
        collect: F,
    ) -> Result<CollectNetworkInfoResponse, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<CollectNetworkInfoResponse, E>>,
    {
        self.prune_idle();
        let slot = self.slots.entry((user_id, machine_id)).or_default().clone();
        let mut cached = slot.lock().await;
        if let Some((collected_at, response)) = cached.as_ref()
            && collected_at.elapsed() < FRESH_FOR
        {
            return Ok(response.clone());
        }
        let response = collect().await?;
        *cached = Some((Instant::now(), response.clone()));
        Ok(response)
    }

    fn prune_idle(&self) {
        self.slots.retain(|_, slot| {
            slot.try_lock().map_or(true, |cached| {
                cached
                    .as_ref()
                    .is_some_and(|(collected_at, _)| collected_at.elapsed() < IDLE_FOR)
            })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn response(name: &str) -> CollectNetworkInfoResponse {
        let mut response = CollectNetworkInfoResponse::default();
        response.info.get_or_insert_default().map.insert(
            name.to_owned(),
            easytier::proto::api::manage::NetworkInstanceRunningInfo::default(),
        );
        response
    }

    #[tokio::test]
    async fn concurrent_viewers_share_one_collection() {
        let cache = NetworkInfoCache::default();
        let calls = AtomicUsize::new(0);
        let machine = uuid::Uuid::new_v4();
        let collect = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok::<_, ()>(response("a"))
        };
        let (left, right) = tokio::join!(
            cache.get_or_collect(1, machine, collect),
            cache.get_or_collect(1, machine, collect),
        );
        assert_eq!(left.unwrap(), right.unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // A fresh result is reused; another user or machine is collected.
        cache.get_or_collect(1, machine, collect).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        cache.get_or_collect(2, machine, collect).await.unwrap();
        cache
            .get_or_collect(1, uuid::Uuid::new_v4(), collect)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn failures_are_not_cached_and_results_expire() {
        let cache = NetworkInfoCache::default();
        let machine = uuid::Uuid::new_v4();
        assert!(
            cache
                .get_or_collect(1, machine, || async {
                    Err::<CollectNetworkInfoResponse, _>("offline")
                })
                .await
                .is_err()
        );
        let first = cache
            .get_or_collect(1, machine, || async { Ok::<_, ()>(response("a")) })
            .await
            .unwrap();
        assert_eq!(first, response("a"));
        let slot = cache.slots.get(&(1, machine)).unwrap().clone();
        slot.lock().await.as_mut().unwrap().0 -= FRESH_FOR;
        let second = cache
            .get_or_collect(1, machine, || async { Ok::<_, ()>(response("b")) })
            .await
            .unwrap();
        assert_eq!(second, response("b"));
    }
}
