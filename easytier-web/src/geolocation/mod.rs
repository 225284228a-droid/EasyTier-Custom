mod provider;
mod store;

use std::{
    collections::{BTreeSet, HashMap},
    net::IpAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use easytier::proto::api::manage::MyNodeInfo;
use provider::{Provider, ProviderError};
use store::{ProviderPermit, Store};
use tokio::{sync::Notify, task::JoinSet};

const ACTIVE_WINDOW_SECONDS: i64 = 180;
const MAX_STALE_SECONDS: i64 = 7 * 86_400;
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(3600);
const REQUEST_SQL_TIMEOUT: Duration = Duration::from_millis(150);
const STORE_TOUCH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, clap::Args)]
pub(crate) struct Options {
    /// Independent SQLite file for online city-level IP geolocation.
    #[arg(
        long = "geoip-cache-db",
        env = "ET_GEOIP_CACHE_DB",
        default_value = "geoip-cache.db"
    )]
    pub database: PathBuf,

    /// Disable external geolocation requests; keep local/cache lookups.
    #[arg(
        long = "geoip-offline",
        env = "ET_GEOIP_OFFLINE",
        default_value = "false"
    )]
    pub offline: bool,

    /// Refresh geolocation for actively used node IPs after this many hours.
    #[arg(long = "geoip-refresh-hours", env = "ET_GEOIP_REFRESH_HOURS", default_value = "24",
        value_parser = clap::value_parser!(u32).range(1..=168))]
    pub refresh_hours: u32,

    /// Delete records unused for this many days.
    #[arg(long = "geoip-retention-days", env = "ET_GEOIP_RETENTION_DAYS", default_value = "30",
        value_parser = clap::value_parser!(u32).range(1..=365))]
    pub retention_days: u32,

    /// Maximum cached IP records; least-recently-used records are evicted.
    #[arg(long = "geoip-cache-max-entries", env = "ET_GEOIP_CACHE_MAX_ENTRIES",
        default_value = "10000", value_parser = clap::value_parser!(u32).range(1..=1_000_000))]
    pub max_entries: u32,

    /// Maximum requests per provider per UTC day, persisted across restarts.
    #[arg(long = "geoip-daily-budget", env = "ET_GEOIP_DAILY_BUDGET", default_value = "900",
        value_parser = clap::value_parser!(u32).range(1..=900))]
    pub daily_budget: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            database: PathBuf::from("geoip-cache.db"),
            offline: false,
            refresh_hours: 24,
            retention_days: 30,
            max_entries: 10_000,
            daily_budget: 900,
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct CityLocation {
    pub country: String,
    pub city: String,
    pub region: Option<String>,
    pub latitude: f64,
    pub longitude: f64,
    pub accuracy_radius_km: Option<f64>,
}

impl CityLocation {
    fn valid(&self) -> bool {
        !self.country.trim().is_empty()
            && !self.city.trim().is_empty()
            && self.latitude.is_finite()
            && self.longitude.is_finite()
            && self.latitude.abs() <= 90.0
            && self.longitude.abs() <= 180.0
            && self
                .accuracy_radius_km
                .is_none_or(|radius| radius.is_finite() && radius >= 0.0)
    }

    pub(crate) fn into_location(self) -> crate::client_manager::session::Location {
        crate::client_manager::session::Location {
            country: self.country,
            city: Some(self.city),
            region: self.region,
            latitude: Some(self.latitude),
            longitude: Some(self.longitude),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CachedCity {
    pub location: CityLocation,
    pub source: String,
    pub updated_at: i64,
}

#[derive(Debug)]
struct MemoryEntry {
    city: Option<CachedCity>,
    last_observed_unix: i64,
    last_store_touch: Instant,
    loaded_at: Option<Instant>,
    order: u64,
}

#[derive(Debug, Default)]
struct MemoryCache {
    entries: HashMap<IpAddr, MemoryEntry>,
    order: BTreeSet<(u64, IpAddr)>,
    next_order: u64,
}

impl MemoryCache {
    fn remove(&mut self, ip: IpAddr) {
        if let Some(entry) = self.entries.remove(&ip) {
            self.order.remove(&(entry.order, ip));
        }
    }

    fn trim(&mut self, max_entries: u32) {
        while self.entries.len() > max_entries as usize {
            let Some((_, ip)) = self.order.first().copied() else {
                break;
            };
            self.remove(ip);
        }
    }

    fn update_city(&mut self, ip: IpAddr, city: Option<CachedCity>, loaded_at: Instant) {
        let Some(entry) = self.entries.get_mut(&ip) else {
            return;
        };
        if let Some(city) = city
            && entry
                .city
                .as_ref()
                .is_none_or(|previous| city.updated_at >= previous.updated_at)
        {
            entry.city = Some(city);
        }
        entry.loaded_at = Some(loaded_at);
    }
}

#[derive(Debug)]
pub(crate) struct CityGeoCache {
    store: Store,
    client: reqwest::Client,
    options: Options,
    wake: Notify,
    memory: Mutex<MemoryCache>,
    #[cfg(test)]
    test_endpoints: Option<[url::Url; 2]>,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub(crate) fn usable_node_ip(ip: &IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            !ip.is_unspecified()
                && !ip.is_loopback()
                && !ip.is_private()
                && !ip.is_link_local()
                && !ip.is_multicast()
                && !ip.is_broadcast()
                && octets[0] != 0
                && octets[0] < 240
                && !(octets[0] == 100 && (64..128).contains(&octets[1]))
        }
        IpAddr::V6(ip) => {
            !ip.is_unspecified()
                && !ip.is_loopback()
                && !ip.is_unique_local()
                && !ip.is_unicast_link_local()
                && !ip.is_multicast()
        }
    }
}

fn online_lookup_ip(ip: &IpAddr) -> bool {
    usable_node_ip(ip)
        && match ip.to_canonical() {
            IpAddr::V4(ip) => !ip.is_documentation(),
            IpAddr::V6(ip) => (ip.segments()[0], ip.segments()[1]) != (0x2001, 0x0db8),
        }
}

pub(crate) fn node_public_ip(node: &MyNodeInfo) -> Option<IpAddr> {
    node.stun_info
        .as_ref()
        .and_then(|stun| {
            stun.public_ip
                .iter()
                .filter_map(|value| value.parse::<IpAddr>().ok())
                .find(usable_node_ip)
        })
        .or_else(|| {
            node.ips
                .as_ref()
                .and_then(|ips| ips.public_ipv4)
                .map(std::net::Ipv4Addr::from)
                .map(IpAddr::V4)
                .filter(usable_node_ip)
        })
        .or_else(|| {
            node.ips
                .as_ref()
                .and_then(|ips| ips.public_ipv6)
                .map(std::net::Ipv6Addr::from)
                .map(IpAddr::V6)
                .filter(usable_node_ip)
        })
        .map(|ip| ip.to_canonical())
}

fn ensure_separate_database(path: &std::path::Path, primary_db: &str) -> Result<()> {
    let primary = primary_db.parse::<sqlx::sqlite::SqliteConnectOptions>()?;
    if let (Ok(primary), Ok(cache)) = (primary.get_filename().canonicalize(), path.canonicalize())
        && primary == cache
    {
        bail!("the city geolocation cache must not use the primary database file");
    }
    Ok(())
}

impl CityGeoCache {
    pub(crate) fn online_enabled(&self) -> bool {
        !self.options.offline
    }

    pub(crate) async fn open(options: Options, primary_db: &str) -> Result<Arc<Self>> {
        ensure_separate_database(&options.database, primary_db)?;
        let store = Store::open(&options.database).await?;
        Ok(Arc::new(Self::from_store(store, options)?))
    }

    fn from_store(store: Store, options: Options) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(8))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("EasyTier-Custom city-geolocation")
            .build()?;
        Ok(Self {
            store,
            client,
            options,
            wake: Notify::new(),
            memory: Mutex::new(MemoryCache::default()),
            #[cfg(test)]
            test_endpoints: None,
        })
    }

    #[cfg(test)]
    pub(crate) async fn test_memory(options: Options) -> Arc<Self> {
        Arc::new(Self::from_store(Store::memory().await.unwrap(), options).unwrap())
    }

    #[cfg(test)]
    pub(crate) async fn test_seed(&self, ip: IpAddr, location: &CityLocation, updated_at: i64) {
        self.store
            .observe(ip, updated_at, self.options.max_entries)
            .await
            .unwrap();
        self.save_city(ip, location.clone(), "geojs", updated_at)
            .await
            .unwrap();
    }

    #[cfg(test)]
    pub(crate) async fn test_close(&self) {
        self.store.test_pool().close().await;
    }

    fn usable_cached(city: CachedCity, at: i64) -> Option<CachedCity> {
        (city.location.valid() && at.saturating_sub(city.updated_at) <= MAX_STALE_SECONDS)
            .then_some(city)
    }

    /// Called only for an IP confirmed by a recent mesh-reported snapshot.
    pub(crate) async fn observe(self: &Arc<Self>, ip: IpAddr) -> Option<CachedCity> {
        if !online_lookup_ip(&ip) {
            return None;
        }
        let ip = ip.to_canonical();
        let at = now();
        let instant = Instant::now();
        let (cached, touch, load) = {
            let mut memory = self.memory.lock().expect("city memory cache poisoned");
            memory.next_order = memory.next_order.wrapping_add(1);
            let order = memory.next_order;
            if let Some(mut entry) = memory.entries.remove(&ip) {
                memory.order.remove(&(entry.order, ip));
                entry.last_observed_unix = at;
                entry.order = order;
                let cached = entry
                    .city
                    .clone()
                    .and_then(|city| Self::usable_cached(city, at));
                let ready = cached.is_some()
                    || entry.loaded_at.is_some_and(|loaded| {
                        instant.saturating_duration_since(loaded) < STORE_TOUCH_INTERVAL
                    });
                let touch = instant.saturating_duration_since(entry.last_store_touch)
                    >= STORE_TOUCH_INTERVAL;
                if touch {
                    entry.last_store_touch = instant;
                }
                memory.order.insert((order, ip));
                memory.entries.insert(ip, entry);
                (cached, touch, touch && !ready)
            } else if self.options.max_entries > 0 {
                memory.entries.insert(
                    ip,
                    MemoryEntry {
                        city: None,
                        last_observed_unix: at,
                        last_store_touch: instant,
                        loaded_at: None,
                        order,
                    },
                );
                memory.order.insert((order, ip));
                memory.trim(self.options.max_entries);
                (None, true, true)
            } else {
                (None, false, false)
            }
        };
        if load {
            return self.observe_store(ip, at).await.or(cached);
        }
        if touch {
            let cache = self.clone();
            tokio::spawn(async move {
                cache.observe_store(ip, at).await;
            });
        }
        cached
    }

    async fn observe_store(&self, ip: IpAddr, at: i64) -> Option<CachedCity> {
        let city = match tokio::time::timeout(
            REQUEST_SQL_TIMEOUT,
            self.store.observe(ip, at, self.options.max_entries),
        )
        .await
        {
            Ok(Ok(city)) => {
                if !self.options.offline {
                    self.wake.notify_one();
                }
                city.and_then(|city| Self::usable_cached(city, at))
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "city geolocation cache lookup failed");
                return self.remember_after_sql_failure(ip, at);
            }
            Err(_) => {
                tracing::debug!("city geolocation cache lookup timed out");
                return self.remember_after_sql_failure(ip, at);
            }
        };
        self.memory
            .lock()
            .expect("city memory cache poisoned")
            .update_city(ip, city.clone(), Instant::now());
        city
    }

    fn remember_after_sql_failure(&self, ip: IpAddr, at: i64) -> Option<CachedCity> {
        let mut memory = self.memory.lock().expect("city memory cache poisoned");
        memory.update_city(ip, None, Instant::now());
        memory
            .entries
            .get(&ip)
            .and_then(|entry| entry.city.clone())
            .and_then(|city| Self::usable_cached(city, at))
    }

    pub(crate) async fn peek(&self, ip: IpAddr) -> Option<CachedCity> {
        let ip = ip.to_canonical();
        let at = now();
        {
            let memory = self.memory.lock().expect("city memory cache poisoned");
            if let Some(entry) = memory.entries.get(&ip) {
                if let Some(city) = entry
                    .city
                    .clone()
                    .and_then(|city| Self::usable_cached(city, at))
                {
                    return Some(city);
                }
                if entry.city.is_none()
                    && entry
                        .loaded_at
                        .is_some_and(|loaded| loaded.elapsed() < STORE_TOUCH_INTERVAL)
                {
                    return None;
                }
            }
        }
        match tokio::time::timeout(REQUEST_SQL_TIMEOUT, self.store.read(ip)).await {
            Ok(Ok(city)) => {
                let city = city.and_then(|city| Self::usable_cached(city, at));
                self.memory
                    .lock()
                    .expect("city memory cache poisoned")
                    .update_city(ip, city.clone(), Instant::now());
                city
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "city geolocation cache read failed");
                None
            }
            Err(_) => {
                tracing::debug!("city geolocation cache read timed out");
                None
            }
        }
    }

    fn cleanup_memory(&self, at: i64) {
        let cutoff = at.saturating_sub(i64::from(self.options.retention_days) * 86_400);
        let mut memory = self.memory.lock().expect("city memory cache poisoned");
        let expired: Vec<_> = memory
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.last_observed_unix < cutoff
                    || entry
                        .city
                        .as_ref()
                        .is_some_and(|city| Self::usable_cached(city.clone(), at).is_none())
            })
            .map(|(ip, _)| *ip)
            .collect();
        for ip in expired {
            memory.remove(ip);
        }
        memory.trim(self.options.max_entries);
    }

    pub(crate) async fn run(self: Arc<Self>) {
        let mut poll = tokio::time::interval(Duration::from_secs(5));
        let mut cleanup = tokio::time::interval(MAINTENANCE_INTERVAL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        cleanup.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cleanup.tick() => {
                    self.cleanup_memory(now());
                    match self.store.cleanup(
                        now(),
                        i64::from(self.options.retention_days) * 86_400,
                        self.options.max_entries,
                    ).await {
                        Ok(removed) if removed > 0 => {
                            tracing::info!(removed, "removed unused city geolocation records");
                        }
                        Ok(_) => {}
                        Err(error) => tracing::warn!(%error, "city geolocation cleanup failed"),
                    }
                }
                _ = poll.tick() => {}
                _ = self.wake.notified() => {}
            }
            if !self.options.offline
                && let Err(error) = self.refresh_due().await
            {
                tracing::warn!(%error, "city geolocation background refresh failed");
            }
        }
    }

    async fn refresh_due(self: &Arc<Self>) -> Result<()> {
        let at = now();
        let due = self.store.due(at, at - ACTIVE_WINDOW_SECONDS, 4).await?;
        let mut tasks = JoinSet::new();
        for ip in due {
            if !online_lookup_ip(&ip) {
                self.store.defer(ip, at + 86_400).await?;
                continue;
            }
            if !self.store.claim(ip, at, at - ACTIVE_WINDOW_SECONDS).await? {
                continue;
            }
            let cache = self.clone();
            tasks.spawn(async move { cache.refresh_one(ip).await });
        }
        while let Some(result) = tasks.join_next().await {
            result.context("city geolocation worker stopped")??;
        }
        Ok(())
    }

    async fn fetch(&self, provider: Provider, ip: IpAddr) -> Result<CityLocation, ProviderError> {
        #[cfg(test)]
        if let Some(endpoints) = &self.test_endpoints {
            let index = if matches!(provider, Provider::GeoJs) {
                0
            } else {
                1
            };
            return provider
                .fetch_at(&self.client, ip, endpoints[index].clone())
                .await;
        }
        provider.fetch(&self.client, ip).await
    }

    async fn save_city(
        &self,
        ip: IpAddr,
        location: CityLocation,
        source: &str,
        at: i64,
    ) -> Result<()> {
        self.store
            .save_success(
                ip,
                &location,
                source,
                at,
                at + i64::from(self.options.refresh_hours) * 3600,
            )
            .await?;
        self.memory
            .lock()
            .expect("city memory cache poisoned")
            .update_city(
                ip.to_canonical(),
                Some(CachedCity {
                    location,
                    source: source.to_string(),
                    updated_at: at,
                }),
                Instant::now(),
            );
        Ok(())
    }

    async fn refresh_one(&self, ip: IpAddr) -> Result<()> {
        let mut wait_until: Option<i64> = None;
        let mut failed = false;
        let previous = self.store.read(ip).await.ok().flatten();
        let providers = if previous
            .as_ref()
            .is_some_and(|city| city.source == Provider::IpWhoIs.source())
        {
            [Provider::IpWhoIs, Provider::GeoJs]
        } else {
            [Provider::GeoJs, Provider::IpWhoIs]
        };
        for provider in providers {
            match self
                .store
                .reserve_provider(provider.source(), now(), self.options.daily_budget)
                .await?
            {
                ProviderPermit::WaitUntil(at) => {
                    wait_until = Some(wait_until.map_or(at, |previous| previous.min(at)));
                    continue;
                }
                ProviderPermit::Granted => {}
            }
            match self.fetch(provider, ip).await {
                Ok(location) if location.valid() => {
                    let at = now();
                    self.save_city(ip, location, provider.source(), at).await?;
                    return Ok(());
                }
                Err(ProviderError::RateLimited { retry_at }) => {
                    let retry_at = retry_at.max(now() + 5);
                    self.store
                        .block_provider(provider.source(), retry_at)
                        .await?;
                    wait_until =
                        Some(wait_until.map_or(retry_at, |previous| previous.min(retry_at)));
                }
                Ok(_) | Err(ProviderError::Unavailable) => failed = true,
            }
        }
        if !failed && let Some(at) = wait_until {
            self.store.defer(ip, at).await?;
        } else {
            let failures = self.store.failure_count(ip).await?;
            let retry = (900 * (1_i64 << failures.min(6))).min(21_600);
            self.store.save_failure(ip, now(), now() + retry).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn city() -> CityLocation {
        CityLocation {
            country: "New Zealand".into(),
            city: "Auckland".into(),
            region: Some("Auckland".into()),
            latitude: -36.85,
            longitude: 174.76,
            accuracy_radius_km: Some(10.0),
        }
    }

    #[test]
    fn rejects_private_mapped_and_documentation_addresses_for_online_requests() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "::ffff:10.0.0.1",
            "100.64.0.1",
            "fd00::1",
            "169.254.1.1",
            "224.0.0.1",
            "203.0.113.1",
            "2001:db8::1",
        ] {
            assert!(!online_lookup_ip(&ip.parse().unwrap()), "{ip}");
        }
        assert!(online_lookup_ip(&"1.1.1.1".parse().unwrap()));
        assert!(online_lookup_ip(&"2001:4860:4860::8888".parse().unwrap()));
    }

    #[test]
    fn stale_or_invalid_city_results_are_not_used_as_precise_coordinates() {
        let mut result = CachedCity {
            location: city(),
            source: "geojs".into(),
            updated_at: 100,
        };
        assert!(CityGeoCache::usable_cached(result.clone(), 101).is_some());
        assert!(CityGeoCache::usable_cached(result.clone(), 101 + MAX_STALE_SECONDS).is_none());
        result.location.latitude = 999.0;
        assert!(CityGeoCache::usable_cached(result, 101).is_none());
    }

    #[tokio::test]
    async fn offline_observations_never_start_external_queries() {
        let options = Options {
            offline: true,
            ..Options::default()
        };
        let cache =
            Arc::new(CityGeoCache::from_store(Store::memory().await.unwrap(), options).unwrap());
        let ip = "1.1.1.1".parse().unwrap();
        assert!(cache.observe(ip).await.is_none());
        cache.save_city(ip, city(), "geojs", now()).await.unwrap();
        assert_eq!(cache.observe(ip).await.unwrap().location.city, "Auckland");
        assert_eq!(cache.store.failure_count(ip).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn failed_sql_touch_preserves_last_valid_memory_city() {
        let cache = CityGeoCache::test_memory(Options {
            offline: true,
            ..Options::default()
        })
        .await;
        let ip = "1.1.1.1".parse().unwrap();
        cache.observe(ip).await;
        cache.save_city(ip, city(), "geojs", now()).await.unwrap();
        cache.test_close().await;
        assert_eq!(
            cache.observe_store(ip, now()).await.unwrap().location,
            city()
        );
        assert_eq!(cache.peek(ip).await.unwrap().location, city());
    }

    #[tokio::test]
    async fn observations_are_throttled_and_memory_is_bounded_lru() {
        let cache = CityGeoCache::test_memory(Options {
            offline: true,
            max_entries: 2,
            ..Options::default()
        })
        .await;
        let first: IpAddr = "1.1.1.1".parse().unwrap();
        let second: IpAddr = "8.8.8.8".parse().unwrap();
        let third: IpAddr = "9.9.9.9".parse().unwrap();
        cache.observe(first).await;
        sqlx::query("UPDATE city_cache SET last_used = 0 WHERE ip = ?")
            .bind(first.to_string())
            .execute(cache.store.test_pool())
            .await
            .unwrap();
        assert!(cache.observe(first).await.is_none());
        let persisted: i64 = sqlx::query_scalar("SELECT last_used FROM city_cache WHERE ip = ?")
            .bind(first.to_string())
            .fetch_one(cache.store.test_pool())
            .await
            .unwrap();
        assert_eq!(persisted, 0);
        {
            let mut memory = cache.memory.lock().unwrap();
            let entry = memory.entries.get_mut(&first).unwrap();
            entry.last_store_touch = Instant::now() - Duration::from_secs(61);
            entry.loaded_at = Some(Instant::now() - Duration::from_secs(61));
        }
        cache.observe(first).await;
        let persisted: i64 = sqlx::query_scalar("SELECT last_used FROM city_cache WHERE ip = ?")
            .bind(first.to_string())
            .fetch_one(cache.store.test_pool())
            .await
            .unwrap();
        assert!(persisted > 0);
        cache.observe(second).await;
        cache.observe(first).await;
        cache.observe(third).await;
        let memory = cache.memory.lock().unwrap();
        assert_eq!(memory.entries.len(), 2);
        assert_eq!(memory.order.len(), 2);
        assert!(memory.entries.contains_key(&first));
        assert!(memory.entries.contains_key(&third));
        assert!(!memory.entries.contains_key(&second));
    }

    #[tokio::test]
    async fn peek_and_provider_saves_do_not_keep_unused_ips_alive() {
        let cache = CityGeoCache::test_memory(Options {
            offline: true,
            retention_days: 1,
            ..Options::default()
        })
        .await;
        let ip = "1.1.1.1".parse().unwrap();
        cache.observe(ip).await;
        cache.save_city(ip, city(), "geojs", now()).await.unwrap();
        let old = now() - 2 * 86_400;
        sqlx::query("UPDATE city_cache SET last_used = ? WHERE ip = ?")
            .bind(old)
            .bind(ip.to_string())
            .execute(cache.store.test_pool())
            .await
            .unwrap();
        cache
            .memory
            .lock()
            .unwrap()
            .entries
            .get_mut(&ip)
            .unwrap()
            .last_observed_unix = old;
        assert!(cache.peek(ip).await.is_some());
        cache.save_city(ip, city(), "geojs", now()).await.unwrap();
        assert_eq!(
            cache
                .memory
                .lock()
                .unwrap()
                .entries
                .get(&ip)
                .unwrap()
                .last_observed_unix,
            old
        );
        assert!(
            cache
                .store
                .due(now(), now() - ACTIVE_WINDOW_SECONDS, 10)
                .await
                .unwrap()
                .is_empty()
        );
        cache.cleanup_memory(now());
        assert!(cache.memory.lock().unwrap().entries.is_empty());
        assert_eq!(cache.store.cleanup(now(), 86_400, 10).await.unwrap(), 1);
        assert!(cache.peek(ip).await.is_none());
    }

    #[tokio::test]
    async fn restoring_a_city_from_sql_does_not_invent_observation_time() {
        let cache = CityGeoCache::test_memory(Options {
            offline: true,
            ..Options::default()
        })
        .await;
        let ip = "1.1.1.1".parse().unwrap();
        cache.test_seed(ip, &city(), now()).await;
        assert!(cache.peek(ip).await.is_some());
        assert!(cache.memory.lock().unwrap().entries.is_empty());
        sqlx::query("UPDATE city_cache SET last_used = 10 WHERE ip = ?")
            .bind(ip.to_string())
            .execute(cache.store.test_pool())
            .await
            .unwrap();
        assert!(cache.peek(ip).await.is_some());
        let persisted: i64 = sqlx::query_scalar("SELECT last_used FROM city_cache WHERE ip = ?")
            .bind(ip.to_string())
            .fetch_one(cache.store.test_pool())
            .await
            .unwrap();
        assert_eq!(persisted, 10);
    }

    #[tokio::test]
    async fn sql_lock_cannot_hold_request_lookups_for_seconds() {
        let cache = CityGeoCache::test_memory(Options {
            offline: true,
            ..Options::default()
        })
        .await;
        let ip = "1.1.1.1".parse().unwrap();
        let transaction = cache
            .store
            .test_pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(400), cache.observe(ip))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), cache.observe(ip))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), cache.peek(ip))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            tokio::time::timeout(
                Duration::from_millis(400),
                cache.peek("8.8.8.8".parse().unwrap()),
            )
            .await
            .unwrap()
            .is_none()
        );
        transaction.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn provider_success_replaces_negative_memory_without_extending_usage() {
        use axum::{Json, Router, routing::get};
        use tokio_util::task::AbortOnDropHandle;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url: url::Url = format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let app = Router::new().route(
            "/",
            get(|| async {
                Json(serde_json::json!({
                    "ip": "1.1.1.1", "country": "New Zealand", "city": "Auckland",
                    "region": "Auckland", "latitude": "-36.85", "longitude": "174.76",
                }))
            }),
        );
        let _server = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let mut cache =
            CityGeoCache::from_store(Store::memory().await.unwrap(), Options::default()).unwrap();
        cache.test_endpoints = Some([url.clone(), url]);
        let cache = Arc::new(cache);
        let ip = "1.1.1.1".parse().unwrap();
        assert!(cache.observe(ip).await.is_none());
        let observed = now() - 30;
        cache
            .memory
            .lock()
            .unwrap()
            .entries
            .get_mut(&ip)
            .unwrap()
            .last_observed_unix = observed;
        cache.refresh_due().await.unwrap();
        assert_eq!(
            cache.store.read(ip).await.unwrap().unwrap().location.city,
            "Auckland"
        );
        assert!(
            cache
                .store
                .due(now(), now() - ACTIVE_WINDOW_SECONDS, 4)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            cache
                .memory
                .lock()
                .unwrap()
                .entries
                .get(&ip)
                .unwrap()
                .last_observed_unix,
            observed
        );
        let transaction = cache
            .store
            .test_pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .unwrap();
        let cached = tokio::time::timeout(Duration::from_millis(100), cache.peek(ip))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cached.location.city, "Auckland");
        transaction.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn memory_cleanup_removes_precise_locations_past_max_staleness() {
        let cache = CityGeoCache::test_memory(Options {
            offline: true,
            ..Options::default()
        })
        .await;
        let ip = "1.1.1.1".parse().unwrap();
        cache.observe(ip).await;
        cache
            .save_city(ip, city(), "geojs", now() - MAX_STALE_SECONDS - 1)
            .await
            .unwrap();
        assert!(cache.peek(ip).await.is_none());
        cache.cleanup_memory(now());
        assert!(cache.memory.lock().unwrap().entries.is_empty());
    }
}
