#[cfg(test)]
mod listener_tests;
mod local_configs;
mod managed_config;
mod runtime_reconcile;
pub mod session;
pub mod storage;

use std::time::Duration;
use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use dashmap::DashMap;
use easytier::common::config::{ConfigSource, config_source_from_rpc, config_source_to_rpc};
use easytier::proto::{
    api::config::ConfigRpc,
    api::manage::{
        CollectNetworkInfoRequest, CollectNetworkInfoResponse, DeleteNetworkInstanceRequest,
        GetNetworkInstanceConfigRequest, ListNetworkInstanceRequest, MyNodeInfo,
        RemoveNetworkInstanceConfigRequest, RunNetworkInstanceRequest,
        SaveNetworkInstanceConfigRequest, SetNetworkInstanceEnabledRequest, WebClientService,
    },
    rpc_types::controller::BaseController,
    web::{HeartbeatRequest, HeartbeatResponse},
};
use easytier_core::{
    management::remote_client::{
        self, ListNetworkInstanceIdsJsonResp, PersistentConfig, RemoteClientError,
        RemoteClientManager, Storage as RemoteStorage,
    },
    socket::SocketListener,
    tunnel::{Tunnel, web_security},
};
use maxminddb::geoip2;
use session::{Location, ManagedConfigPersistedChange, Session};
use storage::{Storage, StorageToken};
use uuid::Uuid;

use crate::FeatureFlags;
use crate::geolocation::{CityGeoCache, node_public_ips};
use crate::webhook::{ManagedNetworkConfig, SharedWebhookConfig};
use tokio::task::JoinSet;

use crate::db::{Db, UserIdInDb, entity::user_running_network_configs};

pub(crate) use managed_config::ManagedConfigError;

const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(3_500);
const DEFAULT_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(15);
const MIN_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const MAX_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
const MIN_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(120);
const HEARTBEAT_TIMEOUT_MARGIN: Duration = Duration::from_secs(5);
const MAX_PENDING_HANDSHAKES: usize = 4096;

#[derive(Debug, Clone, Copy)]
pub(crate) struct HeartbeatPolicy {
    interval: Duration,
    timeout: Duration,
}

impl Default for HeartbeatPolicy {
    fn default() -> Self {
        Self {
            interval: DEFAULT_HEARTBEAT_INTERVAL,
            timeout: DEFAULT_HEARTBEAT_TIMEOUT,
        }
    }
}

impl HeartbeatPolicy {
    pub(crate) fn from_millis(interval_ms: u64, timeout_ms: u64) -> anyhow::Result<Self> {
        let interval = if interval_ms == 0 {
            DEFAULT_HEARTBEAT_INTERVAL
        } else {
            Duration::from_millis(interval_ms)
        };
        let timeout = Duration::from_millis(timeout_ms);
        if !(MIN_HEARTBEAT_INTERVAL..=MAX_HEARTBEAT_INTERVAL).contains(&interval) {
            anyhow::bail!("heartbeat interval must be between 1000 and 60000 milliseconds");
        }
        if !(MIN_HEARTBEAT_TIMEOUT..=MAX_HEARTBEAT_TIMEOUT).contains(&timeout) {
            anyhow::bail!("heartbeat timeout must be between 5000 and 120000 milliseconds");
        }
        if timeout < interval.saturating_add(HEARTBEAT_TIMEOUT_MARGIN) {
            anyhow::bail!(
                "heartbeat timeout must exceed the interval by at least 5000 milliseconds"
            );
        }
        Ok(Self { interval, timeout })
    }

    fn response(self) -> HeartbeatResponse {
        HeartbeatResponse {
            heartbeat_interval_ms: Some(self.interval.as_millis() as u32),
            heartbeat_timeout_ms: Some(self.timeout.as_millis() as u32),
        }
    }

    fn session_rx_timeout(self) -> Duration {
        Duration::from_secs(30).max(self.timeout.saturating_add(HEARTBEAT_TIMEOUT_MARGIN))
    }

    fn legacy_response_delay(self) -> Duration {
        self.interval.min(DEFAULT_HEARTBEAT_INTERVAL)
    }
}

#[derive(rust_embed::Embed)]
#[folder = "resources/"]
#[include = "dbip-country-lite-2026-10.mmdb"]
struct GeoipDb;

fn load_geoip_db(geoip_db: Option<String>) -> Option<maxminddb::Reader<Vec<u8>>> {
    if let Some(path) = geoip_db {
        match maxminddb::Reader::open_readfile(&path) {
            Ok(reader) => {
                tracing::info!("Successfully loaded GeoIP2 database from {}", path);
                Some(reader)
            }
            Err(err) => {
                tracing::debug!("Failed to load GeoIP2 database from {}: {}", path, err);
                None
            }
        }
    } else {
        let db = GeoipDb::get("dbip-country-lite-2026-10.mmdb").unwrap();
        let reader = maxminddb::Reader::from_source(db.data.to_vec()).ok()?;
        tracing::info!("Successfully loaded GeoIP2 database from embedded file");
        Some(reader)
    }
}

#[derive(Debug)]
pub struct ClientManager {
    tasks: JoinSet<()>,

    listeners_cnt: Arc<AtomicU32>,
    next_session_epoch: Arc<AtomicU64>,

    client_sessions: Arc<DashMap<url::Url, Arc<Session>>>,
    storage: Storage,

    feature_flags: Arc<FeatureFlags>,
    webhook_config: SharedWebhookConfig,

    geoip_db: Arc<Option<maxminddb::Reader<Vec<u8>>>>,
    city_geo_cache: Option<Arc<CityGeoCache>>,
    network_location_refresh_slots: Arc<tokio::sync::Semaphore>,
    heartbeat_policy: HeartbeatPolicy,
}

/// Releases a listener slot on any exit path, including a panic inside the
/// accept loop: without this, a panicking task would leak the slot and
/// `is_running` would stay true forever with the port dead.
struct ListenerCountGuard(Arc<AtomicU32>);

impl Drop for ListenerCountGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl ClientManager {
    pub fn new(
        db: Db,
        geoip_db: Option<String>,
        heartbeat_policy: HeartbeatPolicy,
        feature_flags: Arc<FeatureFlags>,
        webhook_config: SharedWebhookConfig,
    ) -> Self {
        let client_sessions = Arc::new(DashMap::new());
        let sessions: Arc<DashMap<url::Url, Arc<Session>>> = client_sessions.clone();
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                Self::prune_sessions(&sessions).await;
            }
        });
        ClientManager {
            tasks,

            listeners_cnt: Arc::new(AtomicU32::new(0)),
            next_session_epoch: Arc::new(AtomicU64::new(0)),

            client_sessions,
            storage: Storage::new(db),
            feature_flags,
            webhook_config,

            geoip_db: Arc::new(load_geoip_db(geoip_db)),
            city_geo_cache: None,
            network_location_refresh_slots: Arc::new(tokio::sync::Semaphore::new(4)),
            heartbeat_policy,
        }
    }

    pub fn set_city_geo_cache(&mut self, cache: Arc<CityGeoCache>) {
        tracing::info!(
            online = cache.online_enabled(),
            "city geolocation enabled; online mode sends only mesh-reported public IPs to GeoJS/IPWho.is"
        );
        self.city_geo_cache = Some(cache.clone());
        self.tasks.spawn(cache.clone().run());
        let sessions = self.client_sessions.clone();
        let slots = self.network_location_refresh_slots.clone();
        let offline_db = self.geoip_db.clone();
        self.tasks.spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let connected: Vec<_> = sessions
                    .iter()
                    .map(|session| session.value().clone())
                    .collect();
                let mut jobs = JoinSet::new();
                for session in connected {
                    if !session.is_running() {
                        continue;
                    }
                    let active_ips = {
                        let data = session.data();
                        let mut data = data.write().await;
                        if !data.has_running_network() {
                            data.clear_network_location();
                            continue;
                        }
                        data.verified_network_location_ips(std::time::Instant::now())
                            .map(|ips| ips.to_vec())
                            .unwrap_or_default()
                    };
                    for ip in active_ips {
                        cache.observe(ip).await;
                    }
                    if jobs.len() >= 4 {
                        let _ = jobs.join_next().await;
                    }
                    let cache = cache.clone();
                    let slots = slots.clone();
                    let offline_db = offline_db.clone();
                    jobs.spawn(async move {
                        let Ok(_permit) = slots.acquire_owned().await else {
                            return;
                        };
                        if !session
                            .data()
                            .write()
                            .await
                            .claim_network_location_refresh(std::time::Instant::now())
                        {
                            return;
                        }
                        let Ok(Ok(response)) = tokio::time::timeout(
                            Duration::from_secs(5),
                            session.scoped_rpc_client().collect_network_info(
                                BaseController::default(),
                                CollectNetworkInfoRequest::default(),
                            ),
                        )
                        .await
                        else {
                            return;
                        };
                        Self::apply_network_location_snapshot(
                            &session, &response, &cache, offline_db,
                        )
                        .await;
                    });
                }
                while jobs.join_next().await.is_some() {}
            }
        });
    }

    async fn prune_sessions(sessions: &DashMap<url::Url, Arc<Session>>) {
        // Release the map guards before reading session state or stopping RPC tasks.
        let snapshot = sessions
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect::<Vec<_>>();
        for (client_url, session) in snapshot {
            if session.is_running() && !session.is_superseded().await {
                continue;
            }
            // A reconnect may have reused the URL since the snapshot was taken.
            sessions.remove_if(&client_url, |_, current| Arc::ptr_eq(current, &session));
            session.stop().await;
        }
    }

    pub async fn add_listener<L: SocketListener<Accepted = Box<dyn Tunnel>> + 'static>(
        &mut self,
        mut listener: L,
    ) -> Result<url::Url, anyhow::Error> {
        listener.listen().await?;
        let local_url = listener.local_url();
        self.listeners_cnt.fetch_add(1, Ordering::Relaxed);
        let sessions = self.client_sessions.clone();
        let storage = self.storage.weak_ref();
        let listeners_cnt = self.listeners_cnt.clone();
        let next_session_epoch = self.next_session_epoch.clone();
        let geoip_db = self.geoip_db.clone();
        let heartbeat_policy = self.heartbeat_policy;
        let feature_flags = self.feature_flags.clone();
        let webhook_config = self.webhook_config.clone();
        self.tasks.spawn(async move {
            let _slot = ListenerCountGuard(listeners_cnt);
            let mut handshakes = JoinSet::new();
            'accept: loop {
                // Some listeners include a WebSocket upgrade in accept(). Keep
                // that future alive while processing completed handshakes.
                let accepting = listener.accept();
                tokio::pin!(accepting);
                loop {
                    let result = tokio::select! {
                        accepted = &mut accepting, if handshakes.len() < MAX_PENDING_HANDSHAKES => {
                            let Ok(tunnel) = accepted else { break 'accept };
                            handshakes.spawn(web_security::accept_or_upgrade_server_tunnel(tunnel));
                            break;
                        }
                        result = handshakes.join_next(), if !handshakes.is_empty() => {
                            result.unwrap()
                        }
                    };
                    let (tunnel, secure) = match result {
                        Ok(Ok(value)) => value,
                        Ok(Err(error)) => {
                            tracing::warn!(%error, "failed to accept secure tunnel, dropping connection");
                            continue;
                        }
                        Err(error) => {
                            tracing::warn!(%error, "secure tunnel handshake task failed");
                            continue;
                        }
                    };
                    let Some(info) = tunnel.info() else {
                        tracing::warn!("accepted tunnel carries no info, dropping connection");
                        continue;
                    };
                    let Some(remote_addr) = info.remote_addr else {
                        tracing::warn!("accepted tunnel carries no remote addr, dropping connection");
                        continue;
                    };
                    let client_url: url::Url = remote_addr.into();
                    let location = Self::lookup_location(&client_url, geoip_db.clone());
                    tracing::info!(
                        "New session from {:?}, secure: {}, location: {:?}",
                        client_url,
                        secure,
                        location
                    );
                    let mut session = Session::new(
                        storage.clone(),
                        client_url.clone(),
                        location,
                        heartbeat_policy,
                        feature_flags.clone(),
                        webhook_config.clone(),
                        next_session_epoch.fetch_add(1, Ordering::Relaxed) + 1,
                    );
                    session.serve(tunnel).await;
                    let session = Arc::new(session);
                    sessions.insert(client_url, session.clone());
                    session.mark_route_ready();
                }
            }
        });

        Ok(local_url)
    }

    pub fn is_running(&self) -> bool {
        self.listeners_cnt.load(Ordering::Relaxed) > 0
    }

    pub async fn list_sessions(&self) -> Vec<StorageToken> {
        self.storage.list_clients()
    }

    pub async fn list_sessions_by_user_id(&self, user_id: UserIdInDb) -> Vec<StorageToken> {
        self.storage.list_user_client_tokens(user_id)
    }

    pub async fn list_all_sessions(&self) -> Vec<StorageToken> {
        self.storage.list_all_clients()
    }

    pub fn get_session_by_machine_id(
        &self,
        user_id: UserIdInDb,
        machine_id: &uuid::Uuid,
    ) -> Option<Arc<Session>> {
        let c_url = self
            .storage
            .get_client_url_by_machine_id(user_id, machine_id)?;
        self.client_sessions
            .get(&c_url)
            .and_then(|item| item.is_running().then(|| item.value().clone()))
    }

    pub async fn disconnect_session_by_machine_id(
        &self,
        user_id: UserIdInDb,
        machine_id: &uuid::Uuid,
    ) -> bool {
        let Some(client_url) = self
            .storage
            .get_client_url_by_machine_id_with_auth(user_id, machine_id, false)
        else {
            return false;
        };
        let Some((_, session)) = self.client_sessions.remove(&client_url) else {
            return false;
        };
        session.stop().await;
        true
    }

    pub async fn list_machine_by_user_id(&self, user_id: UserIdInDb) -> Vec<url::Url> {
        self.storage.list_user_clients(user_id)
    }

    pub async fn reconcile_managed_network_configs(
        &self,
        user_id: UserIdInDb,
        machine_id: uuid::Uuid,
        desired_configs: Vec<ManagedNetworkConfig>,
        config_revision: Option<String>,
        expected_config_revision: Option<String>,
    ) -> anyhow::Result<()> {
        // Decentralized cores own their configs as local TOML files; the web
        // console must not write/delete managed DB rows for them (that would
        // create orphan rows that a stale reconcile could push back to the
        // core). This flag is only known after the first heartbeat, which is
        // the earliest such a reconcile request can be meaningfully applied.
        if self.supports_local_configs(&(user_id, machine_id)).await {
            tracing::info!(
                user_id,
                %machine_id,
                "core manages local configs; ignoring managed config reconcile"
            );
            return Ok(());
        }
        let expected_config_revision = match expected_config_revision.as_deref().map(str::trim) {
            None => managed_config::ExpectedConfigRevision::Any,
            Some("") => managed_config::ExpectedConfigRevision::Exact(None),
            Some(revision) => managed_config::ExpectedConfigRevision::Exact(Some(revision)),
        };
        let status = managed_config::reconcile_web_source_configs(
            &self.storage,
            user_id,
            machine_id,
            desired_configs,
            config_revision.as_deref(),
            expected_config_revision,
        )
        .await?;
        if matches!(
            status,
            managed_config::ManagedConfigApplyStatus::Applied { .. }
        ) && self.storage.record_full_managed_config_change(
            user_id,
            machine_id,
            config_revision.as_deref(),
        ) && let Some(session) = self.get_session_by_machine_id(user_id, &machine_id)
        {
            session
                .notify_managed_runtime_state_changed(user_id, machine_id)
                .await;
        }
        Ok(())
    }

    pub async fn patch_managed_network_configs(
        &self,
        user_id: UserIdInDb,
        machine_id: uuid::Uuid,
        upserts: Vec<ManagedNetworkConfig>,
        delete_instance_ids: Vec<uuid::Uuid>,
        config_revision: String,
        expected_config_revision: String,
    ) -> anyhow::Result<()> {
        if self.supports_local_configs(&(user_id, machine_id)).await {
            tracing::info!(
                user_id,
                %machine_id,
                "core manages local configs; ignoring managed config patch"
            );
            return Ok(());
        }
        let config_revision = config_revision.trim().to_string();
        let expected_config_revision = expected_config_revision.trim().to_string();
        let mut dirty_instance_ids: HashSet<_> = upserts
            .iter()
            .map(|config| config.instance_id.clone())
            .collect();
        let status = managed_config::patch_web_source_configs(
            &self.storage,
            user_id,
            machine_id,
            upserts,
            delete_instance_ids,
            &config_revision,
            &expected_config_revision,
        )
        .await?;
        if let managed_config::ManagedConfigApplyStatus::Applied {
            deleted_web_instance_ids,
        } = status
        {
            dirty_instance_ids.extend(
                deleted_web_instance_ids
                    .into_iter()
                    .map(|instance_id| instance_id.to_string()),
            );
            let changed = self.storage.record_patch_managed_config_change(
                user_id,
                machine_id,
                ManagedConfigPersistedChange {
                    expected_revision: expected_config_revision,
                    target_revision: config_revision,
                    dirty_instance_ids,
                },
            );
            if changed && let Some(session) = self.get_session_by_machine_id(user_id, &machine_id) {
                session
                    .notify_managed_runtime_state_changed(user_id, machine_id)
                    .await;
            }
        }
        Ok(())
    }

    pub async fn invalidate_applied_config_revision(
        &self,
        user_id: UserIdInDb,
        machine_id: uuid::Uuid,
    ) {
        if self
            .storage
            .invalidate_managed_runtime_state(user_id, machine_id)
            && let Some(session) = self.get_session_by_machine_id(user_id, &machine_id)
        {
            session
                .notify_managed_runtime_state_changed(user_id, machine_id)
                .await;
        }
    }

    pub async fn get_heartbeat_requests(&self, client_url: &url::Url) -> Option<HeartbeatRequest> {
        let s = self.client_sessions.get(client_url)?.clone();
        s.data().read().await.req()
    }

    pub async fn get_machine_location(&self, client_url: &url::Url) -> Option<Location> {
        let s = self.client_sessions.get(client_url)?.clone();
        s.data().read().await.location().cloned()
    }

    pub fn lookup_ip_location(&self, ip: std::net::IpAddr) -> Option<Location> {
        Self::lookup_ip(ip, self.geoip_db.clone())
    }

    pub async fn resolve_ip_location(&self, ip: std::net::IpAddr) -> Option<Location> {
        Self::resolve_location(ip, self.city_geo_cache.as_ref(), self.geoip_db.clone()).await
    }

    pub async fn resolve_node_location(
        &self,
        node: &MyNodeInfo,
    ) -> Option<(std::net::IpAddr, Location)> {
        let ips = node_public_ips(node);
        let public_ip = *ips.first()?;
        let location = Self::resolve_node_ips(
            &ips,
            self.city_geo_cache.as_ref(),
            self.geoip_db.clone(),
            true,
        )
        .await?;
        Some((public_ip, location))
    }

    async fn resolve_location(
        ip: std::net::IpAddr,
        cache: Option<&Arc<CityGeoCache>>,
        offline_db: Arc<Option<maxminddb::Reader<Vec<u8>>>>,
    ) -> Option<Location> {
        Self::node_candidate_location(ip, cache, offline_db, true).await
    }

    async fn node_candidate_location(
        ip: std::net::IpAddr,
        cache: Option<&Arc<CityGeoCache>>,
        offline_db: Arc<Option<maxminddb::Reader<Vec<u8>>>>,
        observe: bool,
    ) -> Option<Location> {
        if let Some(cache) = cache {
            let city = if observe {
                cache.observe(ip).await
            } else {
                cache.peek(ip).await
            };
            if let Some(city) = city {
                return Some(city.location.into_location());
            }
        }
        Self::lookup_ip(ip, offline_db)
    }

    fn city_level_location(location: &Location) -> bool {
        location
            .city
            .as_deref()
            .is_some_and(|city| !city.trim().is_empty())
            && location
                .latitude
                .is_some_and(|value| value.is_finite() && value.abs() <= 90.0)
            && location
                .longitude
                .is_some_and(|value| value.is_finite() && value.abs() <= 180.0)
    }

    fn choose_node_location(first: Option<Location>, second: Option<Location>) -> Option<Location> {
        if first.as_ref().is_some_and(Self::city_level_location) {
            return first;
        }
        if second.as_ref().is_some_and(Self::city_level_location) {
            return second;
        }
        let known_country = |location: &Location| {
            !location.country.trim().is_empty()
                && location.country != "海外"
                && !location.country.eq_ignore_ascii_case("unknown")
        };
        if first.as_ref().is_some_and(known_country) {
            return first;
        }
        if second.as_ref().is_some_and(known_country) {
            return second;
        }
        first.or(second)
    }

    async fn resolve_node_ips(
        ips: &[std::net::IpAddr],
        cache: Option<&Arc<CityGeoCache>>,
        offline_db: Arc<Option<maxminddb::Reader<Vec<u8>>>>,
        observe: bool,
    ) -> Option<Location> {
        let first = *ips.first()?;
        if let Some(second) = ips.get(1) {
            // Observe both families even on an IPv4 hit, so the background
            // worker can recover through the same node's alternate family.
            let (first, second) = tokio::join!(
                Self::node_candidate_location(first, cache, offline_db.clone(), observe),
                Self::node_candidate_location(*second, cache, offline_db, observe),
            );
            return Self::choose_node_location(first, second);
        }
        Self::node_candidate_location(first, cache, offline_db, observe).await
    }

    fn snapshot_public_ips(
        response: &CollectNetworkInfoResponse,
    ) -> Option<Vec<Vec<std::net::IpAddr>>> {
        let info = response.info.as_ref()?;
        if info.map.is_empty() {
            return None;
        }
        let mut addresses: Vec<_> = info
            .map
            .iter()
            .filter(|(_, detail)| detail.running)
            .filter_map(|(instance, detail)| {
                let ips = node_public_ips(detail.my_node_info.as_ref()?);
                (!ips.is_empty()).then_some((instance, ips))
            })
            .collect();
        addresses.sort_by(|(left, left_ips), (right, right_ips)| {
            left_ips[0]
                .is_ipv6()
                .cmp(&right_ips[0].is_ipv6())
                .then_with(|| left.cmp(right))
        });
        Some(addresses.into_iter().map(|(_, ips)| ips).collect())
    }

    async fn apply_network_location_snapshot(
        session: &Arc<Session>,
        response: &CollectNetworkInfoResponse,
        cache: &Arc<CityGeoCache>,
        offline_db: Arc<Option<maxminddb::Reader<Vec<u8>>>>,
    ) {
        {
            let data = session.data();
            let mut data = data.write().await;
            if !data.has_running_network() {
                data.clear_network_location();
                return;
            }
        }
        // An empty metadata response can be transient while heartbeats still
        // report running networks. It never extends the verification grace.
        let Some(ips) = Self::snapshot_public_ips(response) else {
            return;
        };
        let verified_at = std::time::Instant::now();
        let mut primary = None;
        for ips in ips {
            let location =
                Self::resolve_node_ips(&ips, Some(cache), offline_db.clone(), true).await;
            if primary.is_none() {
                primary = location.map(|location| (ips, location));
            }
        }
        let data = session.data();
        let mut data = data.write().await;
        data.apply_network_location_snapshot(primary, verified_at);
    }

    #[cfg(test)]
    async fn cached_location(
        ip: std::net::IpAddr,
        cache: &Arc<CityGeoCache>,
        offline_db: Arc<Option<maxminddb::Reader<Vec<u8>>>>,
    ) -> Option<Location> {
        Self::node_candidate_location(ip, Some(cache), offline_db, false).await
    }

    pub async fn cached_network_location(
        &self,
        client_url: &url::Url,
    ) -> Option<(String, Location)> {
        let session = self.client_sessions.get(client_url)?.clone();
        if let Some(cache) = &self.city_geo_cache {
            let (cached, ips) = {
                let data = session.data();
                let data = data.read().await;
                let verified_at = std::time::Instant::now();
                let cached = data.verified_network_location(verified_at).cloned()?;
                let ips = data.verified_network_location_ips(verified_at)?.to_vec();
                (cached, ips)
            };
            let location =
                Self::resolve_node_ips(&ips, Some(cache), self.geoip_db.clone(), false).await?;
            return Some((cached.0, location));
        }
        session.data().read().await.network_location().cloned()
    }

    pub async fn cache_network_location(
        &self,
        user_id: UserIdInDb,
        machine_id: uuid::Uuid,
        node: &MyNodeInfo,
        location: Location,
    ) {
        if let Some(session) = self.get_session_by_machine_id(user_id, &machine_id) {
            let data = session.data();
            let mut data = data.write().await;
            if data.has_running_network() {
                data.set_node_network_location_at(
                    node_public_ips(node),
                    location,
                    std::time::Instant::now(),
                );
            }
        }
    }

    pub async fn begin_network_location_refresh(
        &self,
        user_id: UserIdInDb,
        machine_id: uuid::Uuid,
    ) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let permit = self
            .network_location_refresh_slots
            .clone()
            .try_acquire_owned()
            .ok()?;
        let session = self.get_session_by_machine_id(user_id, &machine_id)?;
        if session
            .data()
            .write()
            .await
            .claim_network_location_refresh(std::time::Instant::now())
        {
            Some(permit)
        } else {
            None
        }
    }

    fn db(&self) -> &Db {
        self.storage.db()
    }

    fn lookup_location(
        client_url: &url::Url,
        geoip_db: Arc<Option<maxminddb::Reader<Vec<u8>>>>,
    ) -> Option<Location> {
        let host = client_url.host_str()?;
        let ip: std::net::IpAddr = if let Ok(ip) = host.parse() {
            ip
        } else {
            tracing::debug!("Failed to parse host as IP address: {}", host);
            return None;
        };
        Self::lookup_ip(ip, geoip_db)
    }

    fn lookup_ip(
        ip: std::net::IpAddr,
        geoip_db: Arc<Option<maxminddb::Reader<Vec<u8>>>>,
    ) -> Option<Location> {
        // Skip lookup for private/special IPs
        let is_private = match ip {
            std::net::IpAddr::V4(ipv4) => {
                ipv4.is_private() || ipv4.is_loopback() || ipv4.is_unspecified()
            }
            std::net::IpAddr::V6(ipv6) => ipv6.is_loopback() || ipv6.is_unspecified(),
        };

        if is_private {
            tracing::debug!("Skipping GeoIP lookup for special IP: {}", ip);
            let location = Location {
                country: "本地网络".to_string(),
                city: None,
                region: None,
                latitude: None,
                longitude: None,
            };
            return Some(location);
        }

        let location = if let Some(db) = &*geoip_db {
            match db
                .lookup(ip)
                .and_then(|result| result.decode::<geoip2::City>())
            {
                Ok(Some(city)) => {
                    let country = city
                        .country
                        .names
                        .simplified_chinese
                        .or(city.country.names.english)
                        .or(city.country.iso_code)
                        .unwrap_or("海外")
                        .to_string();
                    let city_name = city
                        .city
                        .names
                        .simplified_chinese
                        .or(city.city.names.english)
                        .map(str::to_string);
                    let region = (!city.subdivisions.is_empty()).then(|| {
                        city.subdivisions
                            .iter()
                            .filter_map(|subdivision| {
                                subdivision
                                    .names
                                    .simplified_chinese
                                    .or(subdivision.names.english)
                            })
                            .collect::<Vec<_>>()
                            .join(",")
                    });

                    Location {
                        country,
                        city: city_name,
                        region,
                        latitude: city.location.latitude,
                        longitude: city.location.longitude,
                    }
                }
                Ok(None) => Location {
                    country: "海外".to_string(),
                    city: None,
                    region: None,
                    latitude: None,
                    longitude: None,
                },
                Err(err) => {
                    tracing::debug!("GeoIP lookup failed for {}: {}", ip, err);
                    Location {
                        country: "海外".to_string(),
                        city: None,
                        region: None,
                        latitude: None,
                        longitude: None,
                    }
                }
            }
        } else {
            tracing::debug!(
                "GeoIP database not available, using default location for {}",
                ip
            );
            Location {
                country: "海外".to_string(),
                city: None,
                region: None,
                latitude: None,
                longitude: None,
            }
        };

        Some(location)
    }
}

#[async_trait]
impl
    RemoteClientManager<
        (UserIdInDb, uuid::Uuid),
        user_running_network_configs::Model,
        sea_orm::DbErr,
    > for ClientManager
{
    fn get_rpc_client(
        &self,
        (user_id, machine_id): (UserIdInDb, uuid::Uuid),
    ) -> Option<Box<dyn WebClientService<Controller = BaseController> + Send>> {
        let s = self.get_session_by_machine_id(user_id, &machine_id)?;
        Some(s.scoped_rpc_client())
    }

    fn get_config_rpc_client(
        &self,
        (user_id, machine_id): (UserIdInDb, uuid::Uuid),
    ) -> Option<Box<dyn ConfigRpc<Controller = BaseController> + Send>> {
        let session = self.get_session_by_machine_id(user_id, &machine_id)?;
        Some(session.scoped_config_client())
    }

    fn get_storage(
        &self,
    ) -> &impl remote_client::Storage<
        (UserIdInDb, uuid::Uuid),
        user_running_network_configs::Model,
        sea_orm::DbErr,
    > {
        self
    }

    async fn handle_collect_network_info(
        &self,
        identify: (UserIdInDb, uuid::Uuid),
        inst_ids: Option<Vec<uuid::Uuid>>,
    ) -> Result<CollectNetworkInfoResponse, RemoteClientError<sea_orm::DbErr>> {
        let full_snapshot = inst_ids.as_ref().is_none_or(Vec::is_empty);
        let client = self
            .get_rpc_client(identify)
            .ok_or(RemoteClientError::ClientNotFound)?;
        let response = client
            .collect_network_info(
                BaseController::default(),
                CollectNetworkInfoRequest {
                    inst_ids: inst_ids
                        .unwrap_or_default()
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                },
            )
            .await?;
        if full_snapshot
            && let Some(cache) = &self.city_geo_cache
            && let Some(session) = self.get_session_by_machine_id(identify.0, &identify.1)
        {
            Self::apply_network_location_snapshot(
                &session,
                &response,
                cache,
                self.geoip_db.clone(),
            )
            .await;
        }
        Ok(response)
    }

    async fn handle_list_network_instance_ids(
        &self,
        identify: (UserIdInDb, uuid::Uuid),
    ) -> Result<ListNetworkInstanceIdsJsonResp, RemoteClientError<sea_orm::DbErr>> {
        if !self.supports_local_configs(&identify).await {
            let client = self
                .get_rpc_client(identify)
                .ok_or(RemoteClientError::ClientNotFound)?;
            let ret = client
                .list_network_instance(BaseController::default(), ListNetworkInstanceRequest {})
                .await?;

            let running_inst_ids = ret.inst_ids.clone().into_iter().collect();

            // collect networks that are disabled
            let disabled_inst_ids = self
                .get_storage()
                .list_network_configs(identify, remote_client::ListNetworkProps::All)
                .await
                .map_err(RemoteClientError::PersistentError)?
                .iter()
                .map(|x| {
                    Into::<easytier::proto::common::Uuid>::into(x.get_network_inst_id().to_string())
                })
                .filter(|id| !ret.inst_ids.contains(id))
                .collect::<Vec<_>>();

            return Ok(ListNetworkInstanceIdsJsonResp {
                running_inst_ids,
                disabled_inst_ids,
                runtime_capabilities: ret.runtime_capabilities,
            });
        }

        let client = self
            .get_rpc_client(identify)
            .ok_or(RemoteClientError::ClientNotFound)?;
        let ret = client
            .list_network_instance(BaseController::default(), ListNetworkInstanceRequest {})
            .await?;
        Ok(ListNetworkInstanceIdsJsonResp {
            running_inst_ids: ret.inst_ids.into_iter().collect(),
            disabled_inst_ids: ret.disabled_inst_ids.into_iter().collect(),
            runtime_capabilities: ret.runtime_capabilities,
        })
    }

    async fn handle_run_network_instance_with_source(
        &self,
        identify: (UserIdInDb, uuid::Uuid),
        config: easytier::proto::api::manage::NetworkConfig,
        save: bool,
        source: ConfigSource,
    ) -> Result<(), RemoteClientError<sea_orm::DbErr>> {
        let _mutation = self
            .storage
            .runtime_mutation_lock(identify)
            .lock_owned()
            .await;
        if !self.supports_local_configs(&identify).await {
            let client = self
                .get_rpc_client(identify)
                .ok_or(RemoteClientError::ClientNotFound)?;
            let resp = client
                .run_network_instance(
                    BaseController::default(),
                    RunNetworkInstanceRequest {
                        inst_id: None,
                        config: Some(config.clone()),
                        overwrite: true,
                        source: config_source_to_rpc(source),
                    },
                )
                .await?;

            if save {
                self.get_storage()
                    .insert_or_update_user_network_config(
                        identify,
                        resp.inst_id.unwrap_or_default().into(),
                        config,
                        source,
                    )
                    .await
                    .map_err(RemoteClientError::PersistentError)?;
            }
            return Ok(());
        }

        let client = self
            .get_rpc_client(identify)
            .ok_or(RemoteClientError::ClientNotFound)?;
        client
            .run_network_instance(
                BaseController::default(),
                RunNetworkInstanceRequest {
                    inst_id: None,
                    config: Some(config),
                    overwrite: true,
                    source: config_source_to_rpc(source),
                },
            )
            .await?;
        // The core persists the config file in its working directory; the web
        // console keeps nothing.
        Ok(())
    }

    async fn handle_remove_network_instances(
        &self,
        identify: (UserIdInDb, uuid::Uuid),
        inst_ids: Vec<uuid::Uuid>,
    ) -> Result<(), RemoteClientError<sea_orm::DbErr>> {
        if inst_ids.is_empty() {
            return Ok(());
        }
        let _mutation = self
            .storage
            .runtime_mutation_lock(identify)
            .lock_owned()
            .await;
        let client = self
            .get_rpc_client(identify)
            .ok_or(RemoteClientError::ClientNotFound)?;
        let local_configs = self.supports_local_configs(&identify).await;
        let complete_delete = local_configs
            && client
                .list_network_instance(BaseController::default(), ListNetworkInstanceRequest {})
                .await?
                .supports_persisted_config_management
                == Some(true);
        let response = client
            .delete_network_instance(
                BaseController::default(),
                DeleteNetworkInstanceRequest {
                    inst_ids: inst_ids.iter().copied().map(Into::into).collect(),

                    ..Default::default()
                },
            )
            .await?;
        let mut remaining: HashSet<Uuid> = response
            .remain_inst_ids
            .into_iter()
            .map(Into::into)
            .filter(|id| inst_ids.contains(id))
            .collect();
        if local_configs && !complete_delete {
            // Older local-config cores only delete running instances. Never
            // pass a refused deletion on to their config-only removal RPC.
            let stopped: Vec<_> = inst_ids
                .iter()
                .filter(|id| !remaining.contains(id))
                .copied()
                .map(Into::into)
                .collect();
            if !stopped.is_empty() {
                let response = client
                    .remove_network_instance_config(
                        BaseController::default(),
                        RemoveNetworkInstanceConfigRequest {
                            inst_ids: stopped,
                            ..Default::default()
                        },
                    )
                    .await?;
                remaining.extend(response.remain_inst_ids.into_iter().map(Uuid::from));
            }
        }
        if !local_configs {
            let removed: Vec<_> = inst_ids
                .iter()
                .filter(|id| !remaining.contains(id))
                .copied()
                .collect();
            self.get_storage()
                .delete_network_configs(identify, &removed)
                .await
                .map_err(RemoteClientError::PersistentError)?;
        }
        if !remaining.is_empty() {
            return Err(RemoteClientError::Other(format!(
                "failed to delete network instances: {}",
                inst_ids
                    .iter()
                    .filter(|id| remaining.contains(id))
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        Ok(())
    }

    async fn handle_update_network_state(
        &self,
        identify: (UserIdInDb, uuid::Uuid),
        inst_id: uuid::Uuid,
        disabled: bool,
    ) -> Result<(), RemoteClientError<sea_orm::DbErr>> {
        let _mutation = self
            .storage
            .runtime_mutation_lock(identify)
            .lock_owned()
            .await;
        let client = self
            .get_rpc_client(identify)
            .ok_or(RemoteClientError::ClientNotFound)?;

        if !self.supports_local_configs(&identify).await {
            let (cfg, source) = self
                .handle_get_network_config_with_source(identify, inst_id)
                .await?;

            if disabled {
                self.get_storage()
                    .insert_or_update_user_network_config(identify, inst_id, cfg.clone(), source)
                    .await
                    .map_err(RemoteClientError::PersistentError)?;

                let response = client
                    .delete_network_instance(
                        BaseController::default(),
                        DeleteNetworkInstanceRequest {
                            inst_ids: vec![inst_id.into()],

                            ..Default::default()
                        },
                    )
                    .await?;
                if response.remain_inst_ids.contains(&inst_id.into()) {
                    return Err(RemoteClientError::Other(format!(
                        "network instance {inst_id} could not be stopped"
                    )));
                }
            } else {
                client
                    .run_network_instance(
                        BaseController::default(),
                        RunNetworkInstanceRequest {
                            inst_id: Some(inst_id.into()),
                            config: Some(cfg),
                            overwrite: true,
                            source: config_source_to_rpc(source),
                        },
                    )
                    .await?;
            }

            self.get_storage()
                .update_network_config_state(identify, inst_id, disabled)
                .await
                .map_err(RemoteClientError::PersistentError)?;
            return Ok(());
        }

        if disabled {
            client
                .set_network_instance_enabled(
                    BaseController::default(),
                    SetNetworkInstanceEnabledRequest {
                        inst_id: Some(inst_id.into()),
                        enabled: false,

                        ..Default::default()
                    },
                )
                .await?;
        } else {
            // Enable restores the instance from its on-disk config file. This
            // mirrors the disable path (which calls set_network_instance_enabled
            // with enabled=false) and avoids the redundant NetworkConfig<->TOML
            // round-trip + file rewrite that run_network_instance(overwrite=true)
            // would perform.
            client
                .set_network_instance_enabled(
                    BaseController::default(),
                    SetNetworkInstanceEnabledRequest {
                        inst_id: Some(inst_id.into()),
                        enabled: true,

                        ..Default::default()
                    },
                )
                .await?;
        }
        Ok(())
    }

    async fn handle_save_network_config_with_source(
        &self,
        identify: (UserIdInDb, uuid::Uuid),
        inst_id: uuid::Uuid,
        config: easytier::proto::api::manage::NetworkConfig,
        source: ConfigSource,
    ) -> Result<(), RemoteClientError<sea_orm::DbErr>> {
        let _mutation = self
            .storage
            .runtime_mutation_lock(identify)
            .lock_owned()
            .await;
        if !self.supports_local_configs(&identify).await {
            self.get_storage()
                .insert_or_update_user_network_config(identify, inst_id, config, source)
                .await
                .map_err(RemoteClientError::PersistentError)?;
            self.get_storage()
                .update_network_config_state(identify, inst_id, true)
                .await
                .map_err(RemoteClientError::PersistentError)?;
            return Ok(());
        }

        let client = self
            .get_rpc_client(identify)
            .ok_or(RemoteClientError::ClientNotFound)?;
        client
            .save_network_instance_config(
                BaseController::default(),
                SaveNetworkInstanceConfigRequest {
                    inst_id: Some(inst_id.into()),
                    config: Some(config),
                    source: config_source_to_rpc(source),
                },
            )
            .await?;
        Ok(())
    }

    async fn handle_get_network_config_with_source(
        &self,
        identify: (UserIdInDb, uuid::Uuid),
        inst_id: uuid::Uuid,
    ) -> Result<
        (easytier::proto::api::manage::NetworkConfig, ConfigSource),
        RemoteClientError<sea_orm::DbErr>,
    > {
        if !self.supports_local_configs(&identify).await {
            if let Some(client) = self.get_rpc_client(identify)
                && let Ok(resp) = client
                    .get_network_instance_config(
                        BaseController::default(),
                        GetNetworkInstanceConfigRequest {
                            inst_id: Some(inst_id.into()),
                        },
                    )
                    .await
                && let Some(config) = resp.config
            {
                let source = if let Some(source) = config_source_from_rpc(resp.source) {
                    source
                } else {
                    self.get_storage()
                        .get_network_config(identify, &inst_id.to_string())
                        .await
                        .map_err(RemoteClientError::PersistentError)?
                        .map(|cfg| cfg.get_runtime_network_config_source())
                        .unwrap_or(ConfigSource::User)
                };
                return Ok((config, source));
            }

            let inst_id = inst_id.to_string();

            let db_row = self
                .get_storage()
                .get_network_config(identify, &inst_id)
                .await
                .map_err(RemoteClientError::PersistentError)?
                .ok_or(RemoteClientError::NotFound(format!(
                    "No such network instance: {}",
                    inst_id
                )))?;

            return Ok((
                db_row
                    .get_network_config()
                    .map_err(RemoteClientError::PersistentError)?,
                db_row.get_runtime_network_config_source(),
            ));
        }

        let client = self
            .get_rpc_client(identify)
            .ok_or(RemoteClientError::ClientNotFound)?;
        let resp = client
            .get_network_instance_config(
                BaseController::default(),
                GetNetworkInstanceConfigRequest {
                    inst_id: Some(inst_id.into()),
                },
            )
            .await?;
        let config = resp.config.ok_or_else(|| {
            RemoteClientError::NotFound(format!("No such network instance: {}", inst_id))
        })?;
        Ok((
            config,
            config_source_from_rpc(resp.source).unwrap_or(ConfigSource::User),
        ))
    }
}

#[async_trait]
impl
    remote_client::Storage<
        (UserIdInDb, uuid::Uuid),
        user_running_network_configs::Model,
        sea_orm::DbErr,
    > for ClientManager
{
    async fn insert_or_update_user_network_config(
        &self,
        identify: (UserIdInDb, uuid::Uuid),
        network_inst_id: Uuid,
        network_config: easytier::proto::api::manage::NetworkConfig,
        source: ConfigSource,
    ) -> Result<(), sea_orm::DbErr> {
        self.db()
            .insert_or_update_user_network_config(identify, network_inst_id, network_config, source)
            .await
    }

    async fn delete_network_configs(
        &self,
        identify: (UserIdInDb, uuid::Uuid),
        network_inst_ids: &[Uuid],
    ) -> Result<(), sea_orm::DbErr> {
        self.db()
            .delete_network_configs(identify, network_inst_ids)
            .await
    }

    async fn update_network_config_state(
        &self,
        identify: (UserIdInDb, uuid::Uuid),
        network_inst_id: Uuid,
        disabled: bool,
    ) -> Result<(), sea_orm::DbErr> {
        self.db()
            .update_network_config_state(identify, network_inst_id, disabled)
            .await
    }

    async fn list_network_configs(
        &self,
        identify: (UserIdInDb, uuid::Uuid),
        props: remote_client::ListNetworkProps,
    ) -> Result<Vec<user_running_network_configs::Model>, sea_orm::DbErr> {
        self.db().list_network_configs(identify, props).await
    }

    async fn get_network_config(
        &self,
        identify: (UserIdInDb, uuid::Uuid),
        network_inst_id: &str,
    ) -> Result<Option<user_running_network_configs::Model>, sea_orm::DbErr> {
        self.db()
            .get_network_config(identify, network_inst_id)
            .await
    }
}

impl ClientManager {
    pub(crate) async fn supports_local_configs(&self, identify: &(UserIdInDb, uuid::Uuid)) -> bool {
        let (user_id, machine_id) = *identify;
        let Some(session) = self.get_session_by_machine_id(user_id, &machine_id) else {
            // Remember legacy custom nodes too; an offline observation never
            // becomes desired state for the official publisher.
            return match self
                .storage
                .db()
                .known_local_config_mode(user_id, machine_id)
                .await
            {
                Ok(local) => local,
                Err(error) => {
                    tracing::warn!(%error, "node management mode unavailable; deferring managed publication");
                    true
                }
            };
        };
        session.data().read().await.support_local_configs()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        future::Future,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use axum::{Json, Router, extract::State, routing::post};
    use easytier::{
        common::{
            MachineIdOptions,
            config::{ConfigSource, NetworkConfigExt},
        },
        instance::factory::{
            NativeInstanceManager, native_compact_instance_manager_with_runtime,
            native_instance_manager,
        },
        proto::{
            api::manage::{NetworkConfig, NetworkingMethod, PortForwardConfig},
            common::CompressionAlgoPb,
            rpc::standalone::{runtime_udp_tunnel_dialer, runtime_udp_tunnel_listener},
        },
        web_client::{WebClient, WebClientOptions, run_web_client},
    };
    use easytier_core::management::InstanceStateStore;
    use easytier_core::management::remote_client::{
        RemoteClientManager as _, Storage as RemoteStorage,
    };
    use serde_json::json;
    use sqlx::Executor;

    use crate::{
        FeatureFlags,
        client_manager::{ClientManager, HeartbeatPolicy, session::Session, storage::StorageToken},
        db::Db,
        webhook::ManagedNetworkConfig,
    };

    const MANAGED_CONFIG_TOKEN: &str = "managed-config-token";

    #[test]
    fn embedded_geoip_location_covers_global_ipv4_and_ipv6() {
        let db = Arc::new(super::load_geoip_db(None));
        let mut countries = std::collections::HashSet::new();
        for ip in ["223.5.5.5", "8.8.8.8", "2001:4860:4860::8888"] {
            let ip = ip.parse().unwrap();
            let country = db
                .as_ref()
                .as_ref()
                .unwrap()
                .lookup(ip)
                .unwrap()
                .decode::<maxminddb::geoip2::Country>()
                .unwrap()
                .unwrap();
            let code = country.country.iso_code.unwrap();
            assert_eq!(code.len(), 2);
            assert!(code.bytes().all(|value| value.is_ascii_uppercase()));
            countries.insert(code.to_string());
            let location = ClientManager::lookup_ip(ip, db.clone()).unwrap();
            assert_ne!(location.country, "海外");
            assert!(location.latitude.is_none());
            assert!(location.longitude.is_none());
        }
        // Anycast locations vary between database releases. Require genuine
        // global coverage instead of pinning their geography to ownership.
        assert!(countries.contains("CN"));
        assert!(countries.len() >= 2);
    }

    #[test]
    fn full_snapshot_distinguishes_transient_empty_metadata_from_missing_ips() {
        use easytier::proto::api::manage::{
            CollectNetworkInfoResponse, MyNodeInfo, NetworkInstanceRunningInfo,
            NetworkInstanceRunningInfoMap,
        };
        let mut response = CollectNetworkInfoResponse::default();
        assert_eq!(ClientManager::snapshot_public_ips(&response), None);
        response.info = Some(NetworkInstanceRunningInfoMap::default());
        assert_eq!(ClientManager::snapshot_public_ips(&response), None);
        response.info.as_mut().unwrap().map.insert(
            "instance".to_string(),
            NetworkInstanceRunningInfo {
                running: true,
                my_node_info: Some(MyNodeInfo::default()),
                ..Default::default()
            },
        );
        assert_eq!(ClientManager::snapshot_public_ips(&response), Some(vec![]));
        response
            .info
            .as_mut()
            .unwrap()
            .map
            .get_mut("instance")
            .unwrap()
            .running = false;
        assert_eq!(ClientManager::snapshot_public_ips(&response), Some(vec![]));
    }

    #[tokio::test]
    async fn stale_or_failed_city_reads_fall_back_to_offline_coordinates() {
        let offline = Arc::new(super::load_geoip_db(None));
        let cache = crate::geolocation::CityGeoCache::test_memory(crate::geolocation::Options {
            offline: true,
            ..Default::default()
        })
        .await;
        let ip = "1.1.1.1".parse().unwrap();
        let city = crate::geolocation::CityLocation {
            country: "New Zealand".to_string(),
            city: "Auckland".to_string(),
            region: None,
            latitude: -36.85,
            longitude: 174.76,
            accuracy_radius_km: None,
        };
        let at = chrono::Utc::now().timestamp();
        cache.test_seed(ip, &city, at).await;
        let fresh = ClientManager::cached_location(ip, &cache, offline.clone())
            .await
            .unwrap();
        assert_eq!(fresh.city.as_deref(), Some("Auckland"));
        assert_eq!(fresh.latitude, Some(-36.85));
        cache.test_seed(ip, &city, at - 8 * 86_400).await;
        let expected = ClientManager::lookup_ip(ip, offline.clone()).unwrap();
        let stale = ClientManager::cached_location(ip, &cache, offline.clone())
            .await
            .unwrap();
        assert_eq!(stale.country, expected.country);
        assert_eq!(stale.city, expected.city);
        assert_eq!(stale.latitude, expected.latitude);
        assert_eq!(stale.longitude, expected.longitude);
        cache.test_close().await;
        let failed = ClientManager::cached_location(ip, &cache, offline)
            .await
            .unwrap();
        assert_eq!(failed.country, expected.country);
        assert_eq!(failed.latitude, expected.latitude);
        assert_eq!(failed.longitude, expected.longitude);
    }

    fn dual_stack_geo_node() -> easytier::proto::api::manage::MyNodeInfo {
        easytier::proto::api::manage::MyNodeInfo {
            stun_info: Some(easytier::proto::common::StunInfo {
                public_ip: vec!["2001:4860:4860::8888".into(), "1.1.1.1".into()],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    async fn dual_stack_geo_manager() -> (ClientManager, Arc<crate::geolocation::CityGeoCache>) {
        let cache = crate::geolocation::CityGeoCache::test_memory(crate::geolocation::Options {
            offline: true,
            ..Default::default()
        })
        .await;
        let mut manager = ClientManager::new(
            Db::memory_db().await,
            None,
            HeartbeatPolicy::default(),
            Arc::new(FeatureFlags::default()),
            Arc::new(crate::webhook::WebhookConfig::new(
                None, None, None, None, None,
            )),
        );
        manager.city_geo_cache = Some(cache.clone());
        (manager, cache)
    }

    fn geo_city(name: &str, latitude: f64) -> crate::geolocation::CityLocation {
        crate::geolocation::CityLocation {
            country: "New Zealand".to_string(),
            city: name.to_string(),
            region: None,
            latitude,
            longitude: 174.76,
            accuracy_radius_km: None,
        }
    }

    #[tokio::test]
    async fn ipv6_city_fallback_keeps_ipv4_display_and_ipv4_city_wins_when_available() {
        let (manager, cache) = dual_stack_geo_manager().await;
        let node = dual_stack_geo_node();
        let v4 = "1.1.1.1".parse().unwrap();
        let v6 = "2001:4860:4860::8888".parse().unwrap();
        cache
            .test_seed(
                v6,
                &geo_city("IPv6 City", -36.85),
                chrono::Utc::now().timestamp(),
            )
            .await;
        let (display, fallback) = manager.resolve_node_location(&node).await.unwrap();
        assert_eq!(display, v4);
        assert_eq!(fallback.city.as_deref(), Some("IPv6 City"));
        assert_eq!(fallback.latitude, Some(-36.85));
        cache
            .test_seed(
                v4,
                &geo_city("IPv4 City", -41.29),
                chrono::Utc::now().timestamp(),
            )
            .await;
        let (display, preferred) = manager.resolve_node_location(&node).await.unwrap();
        assert_eq!(display, v4);
        assert_eq!(preferred.city.as_deref(), Some("IPv4 City"));
        assert_eq!(preferred.latitude, Some(-41.29));
        let cached = ClientManager::resolve_node_ips(
            &crate::geolocation::node_public_ips(&node),
            Some(&cache),
            manager.geoip_db.clone(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(cached.city.as_deref(), Some("IPv4 City"));
    }

    #[tokio::test]
    async fn every_verified_node_family_is_queued_without_cross_node_fallback() {
        let (manager, cache) = dual_stack_geo_manager().await;
        let node = dual_stack_geo_node();
        manager.resolve_node_location(&node).await.unwrap();
        let mut queued = cache.test_due_ips().await;
        queued.sort();
        let mut expected = crate::geolocation::node_public_ips(&node);
        expected.sort();
        assert_eq!(queued, expected);
        cache
            .test_seed(
                "2001:4860:4860::8888".parse().unwrap(),
                &geo_city("Other Node City", -36.85),
                chrono::Utc::now().timestamp(),
            )
            .await;
        let unrelated = easytier::proto::api::manage::MyNodeInfo {
            stun_info: Some(easytier::proto::common::StunInfo {
                public_ip: vec!["9.9.9.9".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let (display, location) = manager.resolve_node_location(&unrelated).await.unwrap();
        assert_eq!(display, "9.9.9.9".parse::<std::net::IpAddr>().unwrap());
        assert_ne!(location.city.as_deref(), Some("Other Node City"));
        assert!(location.latitude.is_none());
        assert!(location.longitude.is_none());
    }

    #[test]
    fn snapshot_keeps_candidates_grouped_by_node_and_ipv4_nodes_first() {
        use easytier::proto::api::manage::{
            CollectNetworkInfoResponse, NetworkInstanceRunningInfo, NetworkInstanceRunningInfoMap,
        };
        let v6_only = easytier::proto::api::manage::MyNodeInfo {
            stun_info: Some(easytier::proto::common::StunInfo {
                public_ip: vec!["2001:4860:4860::8844".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let response = CollectNetworkInfoResponse {
            info: Some(NetworkInstanceRunningInfoMap {
                map: [
                    (
                        "a".to_string(),
                        NetworkInstanceRunningInfo {
                            running: true,
                            my_node_info: Some(v6_only),
                            ..Default::default()
                        },
                    ),
                    (
                        "z".to_string(),
                        NetworkInstanceRunningInfo {
                            running: true,
                            my_node_info: Some(dual_stack_geo_node()),
                            ..Default::default()
                        },
                    ),
                ]
                .into_iter()
                .collect(),
            }),
        };
        let groups = ClientManager::snapshot_public_ips(&response).unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(
            groups[0],
            vec![
                "1.1.1.1".parse::<std::net::IpAddr>().unwrap(),
                "2001:4860:4860::8888".parse().unwrap(),
            ]
        );
        assert_eq!(
            groups[1],
            vec!["2001:4860:4860::8844".parse::<std::net::IpAddr>().unwrap()]
        );
    }

    #[test]
    fn heartbeat_policy_validates_server_configuration() {
        let policy = HeartbeatPolicy::from_millis(3_500, 15_000).unwrap();
        let response = policy.response();
        assert_eq!(response.heartbeat_interval_ms, Some(3_500));
        assert_eq!(response.heartbeat_timeout_ms, Some(15_000));
        assert_eq!(policy.session_rx_timeout(), Duration::from_secs(30));

        let legacy_default = HeartbeatPolicy::from_millis(0, 15_000).unwrap();
        assert_eq!(legacy_default.response().heartbeat_interval_ms, Some(3_500));

        let slow = HeartbeatPolicy::from_millis(60_000, 65_000).unwrap();
        assert_eq!(slow.session_rx_timeout(), Duration::from_secs(70));
        assert_eq!(slow.legacy_response_delay(), Duration::from_millis(3_500));

        assert!(HeartbeatPolicy::from_millis(999, 15_000).is_err());
        assert!(HeartbeatPolicy::from_millis(60_001, 120_000).is_err());
        assert!(HeartbeatPolicy::from_millis(3_500, 4_999).is_err());
        assert!(HeartbeatPolicy::from_millis(60_000, 64_999).is_err());
        assert!(HeartbeatPolicy::from_millis(3_500, 120_001).is_err());
    }

    async fn wait_for_condition<F, Fut>(mut condition: F, timeout: Duration)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = bool>,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        while !condition().await {
            assert!(
                tokio::time::Instant::now() < deadline,
                "condition timed out"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[derive(Debug, Clone)]
    struct TestWebhookState {
        validate_responses: Arc<tokio::sync::Mutex<VecDeque<bool>>>,
        validate_count: Arc<AtomicUsize>,
        block_second_validate: Arc<AtomicBool>,
        allow_second_validate: Arc<AtomicBool>,
        connected_count: Arc<AtomicUsize>,
        block_connected: Arc<AtomicBool>,
        allow_connected: Arc<AtomicBool>,
    }

    impl TestWebhookState {
        fn new(validate_responses: impl IntoIterator<Item = bool>) -> Self {
            Self {
                validate_responses: Arc::new(tokio::sync::Mutex::new(
                    validate_responses.into_iter().collect(),
                )),
                validate_count: Arc::new(AtomicUsize::new(0)),
                block_second_validate: Arc::new(AtomicBool::new(false)),
                allow_second_validate: Arc::new(AtomicBool::new(true)),
                connected_count: Arc::new(AtomicUsize::new(0)),
                block_connected: Arc::new(AtomicBool::new(false)),
                allow_connected: Arc::new(AtomicBool::new(true)),
            }
        }

        fn with_blocked_second_validate(
            validate_responses: impl IntoIterator<Item = bool>,
        ) -> Self {
            let state = Self::new(validate_responses);
            state.block_second_validate.store(true, Ordering::Release);
            state.allow_second_validate.store(false, Ordering::Release);
            state
        }

        fn with_blocked_connected(validate_responses: impl IntoIterator<Item = bool>) -> Self {
            let state = Self::new(validate_responses);
            state.block_connected.store(true, Ordering::Release);
            state.allow_connected.store(false, Ordering::Release);
            state
        }

        fn allow_second_validate(&self) {
            self.allow_second_validate.store(true, Ordering::Release);
        }

        fn validate_count(&self) -> usize {
            self.validate_count.load(Ordering::Acquire)
        }

        fn allow_connected(&self) {
            self.allow_connected.store(true, Ordering::Release);
        }

        fn connected_count(&self) -> usize {
            self.connected_count.load(Ordering::Acquire)
        }
    }

    async fn validate_token_handler(
        State(state): State<TestWebhookState>,
    ) -> Json<serde_json::Value> {
        let count = state.validate_count.fetch_add(1, Ordering::AcqRel) + 1;
        if count == 2 && state.block_second_validate.load(Ordering::Acquire) {
            while !state.allow_second_validate.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        let valid = state
            .validate_responses
            .lock()
            .await
            .pop_front()
            .unwrap_or(true);
        if !valid {
            return Json(json!({ "valid": false }));
        }

        Json(json!({
            "valid": true,
            "binding_version": count,
            "config_revision": format!("validated-rev-{count}")
        }))
    }

    async fn webhook_ack_handler() -> Json<serde_json::Value> {
        Json(json!({}))
    }

    async fn node_connected_handler(
        State(state): State<TestWebhookState>,
    ) -> Json<serde_json::Value> {
        state.connected_count.fetch_add(1, Ordering::AcqRel);
        while state.block_connected.load(Ordering::Acquire)
            && !state.allow_connected.load(Ordering::Acquire)
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Json(json!({}))
    }

    async fn test_webhook_config() -> (
        crate::webhook::SharedWebhookConfig,
        tokio::task::JoinHandle<()>,
        TestWebhookState,
    ) {
        let state = TestWebhookState::new([true]);
        test_webhook_config_with_state(state).await
    }

    async fn test_webhook_config_with_state(
        state: TestWebhookState,
    ) -> (
        crate::webhook::SharedWebhookConfig,
        tokio::task::JoinHandle<()>,
        TestWebhookState,
    ) {
        let app = Router::new()
            .route("/validate-token", post(validate_token_handler))
            .route("/webhook/node-connected", post(node_connected_handler))
            .route("/webhook/node-disconnected", post(webhook_ack_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (
            Arc::new(crate::webhook::WebhookConfig::new(
                Some(format!("http://{addr}")),
                None,
                None,
                None,
                None,
            )),
            server,
            state,
        )
    }

    async fn add_random_udp_listener(mgr: &mut ClientManager) -> std::net::SocketAddr {
        let local_url = "udp://127.0.0.1:0".parse().unwrap();
        let listener = runtime_udp_tunnel_listener(local_url, "127.0.0.1:0".parse().unwrap());
        let local_url = mgr.add_listener(listener).await.unwrap();
        local_url
            .socket_addrs(|| None)
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
    }

    #[tokio::test]
    async fn connected_webhook_observes_a_routable_current_session() {
        let webhook_state = TestWebhookState::with_blocked_connected([true]);
        let (webhook_config, webhook_server, webhook_state) =
            test_webhook_config_with_state(webhook_state).await;
        let mut mgr = ClientManager::new(
            Db::memory_db().await,
            None,
            HeartbeatPolicy::from_millis(0, 15_000).unwrap(),
            Arc::new(FeatureFlags::default()),
            webhook_config,
        );
        let config_server_addr = add_random_udp_listener(&mut mgr).await;
        let machine_id = uuid::Uuid::new_v4();
        let _client = start_web_client_for_test(
            config_server_addr,
            machine_id,
            Arc::new(native_instance_manager()),
        )
        .await;

        wait_for_condition(
            || async { webhook_state.connected_count() == 1 },
            Duration::from_secs(12),
        )
        .await;
        let user_id = wait_for_validated_user(&mgr, machine_id).await;
        let session = mgr
            .get_session_by_machine_id(user_id, &machine_id)
            .expect("connected target must already resolve to a session");
        assert!(session.is_running());

        webhook_state.allow_connected();
        webhook_server.abort();
    }

    #[tokio::test]
    async fn non_running_session_is_not_routable_by_machine_id() {
        let db = Db::memory_db().await;
        let mgr = ClientManager::new(
            db.clone(),
            None,
            HeartbeatPolicy::default(),
            Arc::new(FeatureFlags::default()),
            Arc::new(crate::webhook::WebhookConfig::new(
                None, None, None, None, None,
            )),
        );
        let user_id = db.auto_create_user("token").await.unwrap().id;
        let machine_id = uuid::Uuid::new_v4();
        let client_url = url::Url::parse("udp://127.0.0.1:22020").unwrap();
        mgr.storage.update_client(
            StorageToken {
                token: "token".to_string(),
                client_url: client_url.clone(),
                machine_id,
                user_id,
            },
            1,
            true,
        );
        let session = Arc::new(Session::new(
            mgr.storage.weak_ref(),
            client_url.clone(),
            None,
            HeartbeatPolicy::default(),
            Arc::new(FeatureFlags::default()),
            Arc::new(crate::webhook::WebhookConfig::new(
                None, None, None, None, None,
            )),
            1,
        ));
        assert!(!session.is_running());
        mgr.client_sessions.insert(client_url, session);

        assert!(
            mgr.get_session_by_machine_id(user_id, &machine_id)
                .is_none()
        );
    }

    async fn wait_for_validated_user(mgr: &ClientManager, machine_id: uuid::Uuid) -> i32 {
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                if let Some(token) = mgr.list_sessions().await.into_iter().find(|token| {
                    token.token == MANAGED_CONFIG_TOKEN && token.machine_id == machine_id
                }) {
                    break token.user_id;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap()
    }

    async fn wait_for_local_config_capability(
        mgr: &ClientManager,
        user_id: i32,
        machine_id: uuid::Uuid,
    ) {
        wait_for_condition(
            || async { mgr.supports_local_configs(&(user_id, machine_id)).await },
            Duration::from_secs(12),
        )
        .await;
    }

    fn local_config_instance_manager(config_dir: std::path::PathBuf) -> Arc<NativeInstanceManager> {
        Arc::new(native_instance_manager().with_config_path(Some(config_dir)))
    }

    async fn assert_no_managed_config_state(
        mgr: &ClientManager,
        user_id: i32,
        machine_id: uuid::Uuid,
        instance_id: uuid::Uuid,
    ) {
        assert!(
            mgr.db()
                .get_network_config((user_id, machine_id), &instance_id.to_string())
                .await
                .unwrap()
                .is_none(),
            "local-authority core must not create a managed config row"
        );
        assert!(
            mgr.db()
                .get_managed_config_revision((user_id, machine_id))
                .await
                .unwrap()
                .is_none(),
            "local-authority core must not create a managed config revision"
        );
    }

    async fn wait_for_validate_count(state: &TestWebhookState, target: usize) {
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                if state.validate_count() >= target {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn wait_for_session_urls(mgr: &ClientManager) -> Vec<url::Url> {
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                let urls = mgr
                    .client_sessions
                    .iter()
                    .map(|entry| entry.key().clone())
                    .collect::<Vec<_>>();
                if !urls.is_empty() {
                    break urls;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap()
    }

    fn managed_config(
        instance_id: uuid::Uuid,
        network_config: serde_json::Value,
    ) -> ManagedNetworkConfig {
        ManagedNetworkConfig {
            instance_id: instance_id.to_string(),
            network_config,
        }
    }

    async fn wait_for_runtime_config(
        manager: &NativeInstanceManager,
        inst_id: uuid::Uuid,
        predicate: impl Fn(&NetworkConfig) -> bool,
    ) -> NetworkConfig {
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                if let Some(config) = manager
                    .config(inst_id)
                    .and_then(|config| NetworkConfig::new_from_config(&config).ok())
                    .filter(|config| predicate(config))
                {
                    break config;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap()
    }

    async fn wait_for_applied_revision(
        manager: &ClientManager,
        user_id: i32,
        machine_id: uuid::Uuid,
        revision: &str,
    ) {
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                let applied = manager
                    .get_session_by_machine_id(user_id, &machine_id)
                    .map(|session| async move { session.applied_config_revision().await });
                if let Some(applied) = applied
                    && applied.await.as_deref() == Some(revision)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn start_web_client_for_test(
        config_server_addr: std::net::SocketAddr,
        machine_id: uuid::Uuid,
        manager: Arc<NativeInstanceManager>,
    ) -> WebClient {
        run_web_client(
            &format!("udp://{config_server_addr}/{MANAGED_CONFIG_TOKEN}"),
            MachineIdOptions {
                explicit_machine_id: Some(machine_id.to_string()),
                state_dir: None,
            },
            Some("managed-config-core".to_string()),
            false,
            manager,
            None,
            Arc::new(InstanceStateStore::in_memory()),
        )
        .await
        .unwrap()
    }

    async fn clear_managed_config_db(
        mgr: &ClientManager,
        user_id: i32,
        machine_id: uuid::Uuid,
        instance_id: uuid::Uuid,
    ) {
        mgr.db()
            .delete_web_network_configs((user_id, machine_id), &[instance_id])
            .await
            .unwrap();
        sqlx::query("DELETE FROM managed_config_revisions WHERE user_id = ? AND device_id = ?")
            .bind(user_id)
            .bind(machine_id.to_string())
            .execute(&mgr.db().inner())
            .await
            .unwrap();
    }

    fn assert_updated_runtime_config(updated: &NetworkConfig, instance_id: uuid::Uuid) {
        assert_eq!(
            updated.instance_id.as_deref(),
            Some(instance_id.to_string().as_str())
        );
        assert_eq!(updated.dhcp, Some(false));
        assert_eq!(updated.virtual_ipv4.as_deref(), Some("10.88.0.7"));
        assert_eq!(updated.network_length, Some(24));
        assert_eq!(updated.hostname.as_deref(), Some("managed-updated-host"));
        assert_eq!(updated.network_name.as_deref(), Some("managed-updated"));
        assert_eq!(updated.network_secret.as_deref(), Some("secret-updated"));
        assert_eq!(
            updated.networking_method,
            Some(NetworkingMethod::Manual as i32)
        );
        assert_eq!(updated.peer_urls, vec!["tcp://127.0.0.1:11010".to_string()]);
        assert_eq!(
            updated.proxy_cidrs,
            vec![
                "10.44.0.0/24".to_string(),
                "10.45.0.0/24->10.46.0.0/24".to_string()
            ]
        );
        assert_eq!(updated.no_tun, Some(true));
        assert_eq!(updated.disable_ipv6, Some(true));
        assert_eq!(updated.enable_kcp_proxy, Some(true));
        assert_eq!(updated.disable_kcp_input, Some(true));
        assert_eq!(updated.enable_quic_proxy, Some(true));
        assert_eq!(updated.disable_quic_input, Some(true));
        assert_eq!(updated.disable_p2p, Some(true));
        assert_eq!(updated.p2p_only, Some(true));
        assert_eq!(updated.lazy_p2p, Some(true));
        assert_eq!(updated.relay_all_peer_rpc, Some(true));
        assert_eq!(updated.need_p2p, Some(true));
        assert_eq!(updated.multi_thread, Some(false));
        assert_eq!(updated.proxy_forward_by_system, Some(true));
        assert_eq!(updated.disable_encryption, Some(true));
        assert_eq!(updated.enable_relay_network_whitelist, Some(true));
        assert_eq!(
            updated.relay_network_whitelist,
            vec!["10.44.0.0/24".to_string(), "10.45.0.0/24".to_string()]
        );
        assert_eq!(updated.enable_manual_routes, Some(true));
        assert_eq!(
            updated.routes,
            vec!["10.60.0.0/16".to_string(), "10.61.0.0/16".to_string()]
        );
        assert_eq!(updated.port_forwards[0].bind_ip, "127.0.0.1");
        assert_eq!(updated.port_forwards[0].bind_port, 0);
        assert_eq!(updated.port_forwards[0].dst_ip, "10.88.0.8");
        assert_eq!(updated.port_forwards[0].dst_port, 80);
        assert_eq!(updated.port_forwards[0].proto, "tcp");
        assert_eq!(updated.disable_udp_hole_punching, Some(true));
        assert_eq!(updated.disable_tcp_hole_punching, Some(true));
        assert_eq!(updated.disable_sym_hole_punching, Some(true));
        assert_eq!(updated.disable_upnp, Some(true));
        assert_eq!(updated.disable_relay_data, Some(true));
        assert_eq!(updated.enable_magic_dns, Some(true));
        assert_eq!(updated.enable_private_mode, Some(true));
        assert_eq!(updated.mtu, Some(1360));
        assert_eq!(
            updated.data_compress_algo,
            Some(CompressionAlgoPb::Zstd as i32)
        );
        assert_eq!(updated.encryption_algorithm.as_deref(), Some("xor"));
        assert_eq!(updated.instance_recv_bps_limit, Some(123456));
        assert_eq!(updated.enable_udp_broadcast_relay, Some(true));
        assert_eq!(updated.socket_mark, Some(0));
    }

    fn initial_managed_network_config(inst_id: uuid::Uuid) -> serde_json::Value {
        json!({
            "instance_id": inst_id.to_string(),
            "dhcp": true,
            "network_name": "managed-initial",
            "network_secret": "secret-initial",
            "networking_method": "Standalone",
            "no_tun": true,
            "disable_ipv6": true,
            "enable_kcp_proxy": false,
            "disable_kcp_input": false,
            "relay_all_peer_rpc": false,
            "multi_thread": false,
            "disable_relay_data": false,
            "mtu": 1380
        })
    }

    fn updated_managed_network_config(inst_id: uuid::Uuid) -> serde_json::Value {
        serde_json::to_value(NetworkConfig {
            instance_id: Some(inst_id.to_string()),
            dhcp: Some(false),
            virtual_ipv4: Some("10.88.0.7".to_string()),
            network_length: Some(24),
            hostname: Some("managed-updated-host".to_string()),
            network_name: Some("managed-updated".to_string()),
            network_secret: Some("secret-updated".to_string()),
            networking_method: Some(NetworkingMethod::Manual as i32),
            peer_urls: vec!["tcp://127.0.0.1:11010".to_string()],
            proxy_cidrs: vec![
                "10.44.0.0/24".to_string(),
                "10.45.0.0/24->10.46.0.0/24".to_string(),
            ],
            no_tun: Some(true),
            disable_ipv6: Some(true),
            enable_kcp_proxy: Some(true),
            disable_kcp_input: Some(true),
            enable_quic_proxy: Some(true),
            disable_quic_input: Some(true),
            disable_p2p: Some(true),
            p2p_only: Some(true),
            lazy_p2p: Some(true),
            relay_all_peer_rpc: Some(true),
            need_p2p: Some(true),
            multi_thread: Some(false),
            proxy_forward_by_system: Some(true),
            disable_encryption: Some(true),
            enable_relay_network_whitelist: Some(true),
            relay_network_whitelist: vec!["10.44.0.0/24".to_string(), "10.45.0.0/24".to_string()],
            enable_manual_routes: Some(true),
            routes: vec!["10.60.0.0/16".to_string(), "10.61.0.0/16".to_string()],
            port_forwards: vec![PortForwardConfig {
                bind_ip: "127.0.0.1".to_string(),
                bind_port: 0,
                dst_ip: "10.88.0.8".to_string(),
                dst_port: 80,
                proto: "tcp".to_string(),
            }],
            disable_udp_hole_punching: Some(true),
            disable_tcp_hole_punching: Some(true),
            disable_sym_hole_punching: Some(true),
            disable_upnp: Some(true),
            disable_relay_data: Some(true),
            enable_magic_dns: Some(true),
            enable_private_mode: Some(true),
            mtu: Some(1360),
            data_compress_algo: Some(CompressionAlgoPb::Zstd as i32),
            encryption_algorithm: Some("xor".to_string()),
            instance_recv_bps_limit: Some(123456),
            enable_udp_broadcast_relay: Some(true),
            socket_mark: Some(0),
            ..Default::default()
        })
        .unwrap()
    }

    fn redelivered_managed_network_config(inst_id: uuid::Uuid) -> serde_json::Value {
        serde_json::to_value(NetworkConfig {
            instance_id: Some(inst_id.to_string()),
            dhcp: Some(false),
            virtual_ipv4: Some("10.88.0.7".to_string()),
            network_length: Some(24),
            hostname: Some("managed-redelivered-host".to_string()),
            network_name: Some("managed-redelivered".to_string()),
            network_secret: Some("secret-updated".to_string()),
            networking_method: Some(NetworkingMethod::Manual as i32),
            peer_urls: vec!["tcp://127.0.0.1:11010".to_string()],
            proxy_cidrs: vec![
                "10.44.0.0/24".to_string(),
                "10.45.0.0/24->10.46.0.0/24".to_string(),
            ],
            no_tun: Some(true),
            disable_ipv6: Some(true),
            enable_kcp_proxy: Some(true),
            disable_kcp_input: Some(true),
            relay_all_peer_rpc: Some(true),
            need_p2p: Some(true),
            multi_thread: Some(false),
            enable_private_mode: Some(true),
            mtu: Some(1360),
            data_compress_algo: Some(CompressionAlgoPb::Zstd as i32),
            encryption_algorithm: Some("xor".to_string()),
            instance_recv_bps_limit: Some(654321),
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn test_client() {
        let listener = runtime_udp_tunnel_listener(
            "udp://127.0.0.1:0".parse().unwrap(),
            "127.0.0.1:0".parse().unwrap(),
        );
        let mut mgr = ClientManager::new(
            Db::memory_db().await,
            None,
            HeartbeatPolicy::from_millis(0, 15_000).unwrap(),
            Arc::new(FeatureFlags::default()),
            Arc::new(crate::webhook::WebhookConfig::new(
                None, None, None, None, None,
            )),
        );
        let listener_url = mgr.add_listener(listener).await.unwrap();

        mgr.db()
            .inner()
            .execute("INSERT INTO users (username, password) VALUES ('test', 'test')")
            .await
            .unwrap();

        let connector = runtime_udp_tunnel_dialer(listener_url);
        let _c = WebClient::new(
            connector,
            WebClientOptions {
                token: "test".to_owned(),
                machine_id: uuid::Uuid::new_v4(),
                hostname: "test".to_owned(),
                secure_mode: false,
            },
            Arc::new(native_instance_manager()),
            None,
            Arc::new(InstanceStateStore::in_memory()),
        );

        wait_for_condition(
            || async { !mgr.client_sessions.is_empty() },
            Duration::from_secs(12),
        )
        .await;

        let req = tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                let sessions = mgr
                    .client_sessions
                    .iter()
                    .map(|item| item.value().clone())
                    .collect::<Vec<_>>();
                if sessions.is_empty() {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                let mut found_req = None;
                for session in sessions {
                    if let Some(req) = session.data().read().await.req() {
                        found_req = Some(req);
                        break;
                    }
                }
                if let Some(req) = found_req {
                    break req;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        println!("{:?}", req);
        println!("{:?}", mgr);
    }

    #[tokio::test]
    async fn legacy_delete_keeps_rejected_configs_in_web_storage() {
        use easytier_core::management::{ConfigFileControl, ConfigFilePermission};

        let (webhook_config, webhook_server, _) = test_webhook_config().await;
        let mut mgr = ClientManager::new(
            Db::memory_db().await,
            None,
            HeartbeatPolicy::from_millis(0, 15_000).unwrap(),
            Arc::new(FeatureFlags::default()),
            webhook_config,
        );
        let server_addr = add_random_udp_listener(&mut mgr).await;
        let core_manager = Arc::new(native_instance_manager());
        let machine_id = uuid::Uuid::new_v4();
        let client = start_web_client_for_test(server_addr, machine_id, core_manager.clone()).await;
        let user_id = wait_for_validated_user(&mgr, machine_id).await;
        let identify = (user_id, machine_id);
        wait_for_condition(
            || async { mgr.handle_list_network_instance_ids(identify).await.is_ok() },
            Duration::from_secs(12),
        )
        .await;
        let removable_id = uuid::Uuid::new_v4();
        let protected_id = uuid::Uuid::new_v4();
        for id in [removable_id, protected_id] {
            let config = NetworkConfig {
                instance_id: Some(id.to_string()),
                network_name: Some(format!("legacy-delete-{id}")),
                networking_method: Some(NetworkingMethod::Standalone as i32),
                no_tun: Some(true),
                disable_p2p: Some(true),
                ..Default::default()
            };
            let permission = if id == protected_id {
                ConfigFilePermission::from(ConfigFilePermission::NO_DELETE)
            } else {
                ConfigFilePermission::default()
            };
            core_manager
                .run_network_instance(
                    config.gen_config().unwrap(),
                    ConfigFileControl::new(None, permission),
                )
                .unwrap();
            mgr.insert_or_update_user_network_config(identify, id, config, ConfigSource::User)
                .await
                .unwrap();
        }
        assert!(
            mgr.handle_update_network_state(identify, protected_id, true)
                .await
                .is_err()
        );
        assert!(
            !mgr.get_network_config(identify, &protected_id.to_string())
                .await
                .unwrap()
                .unwrap()
                .disabled
        );
        let error = mgr
            .handle_remove_network_instances(identify, vec![removable_id, protected_id])
            .await
            .unwrap_err();
        assert!(error.to_string().contains(&protected_id.to_string()));
        assert!(!error.to_string().contains(&removable_id.to_string()));
        assert!(
            core_manager.instance(removable_id).is_none(),
            "removable instance survived: {error}"
        );
        assert!(core_manager.instance(protected_id).is_some());
        assert!(
            mgr.get_network_config(identify, &removable_id.to_string())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            mgr.get_network_config(identify, &protected_id.to_string())
                .await
                .unwrap()
                .is_some()
        );

        // A later heartbeat must not resurrect the successfully deleted row.
        let session = mgr.get_session_by_machine_id(user_id, &machine_id).unwrap();
        wait_for_condition(
            || async {
                session.get_heartbeat_req().await.is_some_and(|req| {
                    req.running_network_instances.contains(&protected_id.into())
                        && !req.running_network_instances.contains(&removable_id.into())
                })
            },
            Duration::from_secs(12),
        )
        .await;
        assert!(core_manager.instance(removable_id).is_none());

        client.shutdown().await;
        core_manager.retain_network_instances(&[]).await.unwrap();
        webhook_server.abort();
    }

    #[tokio::test]
    async fn local_config_delete_reports_protected_networks_and_removes_other_configs() {
        use easytier::common::config::{ConfigLoader as _, load_config_from_file};

        let (webhook_config, webhook_server, _) = test_webhook_config().await;
        let mut mgr = ClientManager::new(
            Db::memory_db().await,
            None,
            HeartbeatPolicy::from_millis(0, 15_000).unwrap(),
            Arc::new(FeatureFlags::default()),
            webhook_config,
        );
        let server_addr = add_random_udp_listener(&mut mgr).await;
        let config_dir =
            std::env::temp_dir().join(format!("easytier-web-delete-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&config_dir).unwrap();
        let core_manager = local_config_instance_manager(config_dir.clone());
        let machine_id = uuid::Uuid::new_v4();
        let client = start_web_client_for_test(server_addr, machine_id, core_manager.clone()).await;
        let user_id = wait_for_validated_user(&mgr, machine_id).await;
        wait_for_local_config_capability(&mgr, user_id, machine_id).await;
        let identify = (user_id, machine_id);
        let running_id = uuid::Uuid::new_v4();
        let saved_id = uuid::Uuid::new_v4();
        let protected_id = uuid::Uuid::new_v4();
        let network = |id: uuid::Uuid| NetworkConfig {
            instance_id: Some(id.to_string()),
            network_name: Some(format!("delete-test-{id}")),
            networking_method: Some(NetworkingMethod::Standalone as i32),
            no_tun: Some(true),
            disable_p2p: Some(true),
            ..Default::default()
        };
        mgr.handle_run_network_instance(identify, network(running_id), true)
            .await
            .unwrap();
        mgr.handle_save_network_config(identify, saved_id, network(saved_id))
            .await
            .unwrap();

        let mut protected = network(protected_id);
        protected.network_secret =
            Some("${EASYTIER_WEB_DELETE_TEST_UNSET_SECRET:-protected-secret}".to_string());
        let protected = protected.gen_config().unwrap();
        protected.set_listeners(Vec::new());
        let protected_path = config_dir.join(format!("{protected_id}.toml"));
        std::fs::write(&protected_path, protected.dump()).unwrap();
        let (protected, control) = load_config_from_file(&protected_path, Some(&config_dir), false)
            .await
            .unwrap();
        assert!(control.is_read_only() && control.is_no_delete());
        core_manager
            .run_network_instance(protected, control)
            .unwrap();

        let error = mgr
            .handle_remove_network_instances(identify, vec![running_id, saved_id, protected_id])
            .await
            .unwrap_err();
        assert!(error.to_string().contains(&protected_id.to_string()));
        assert!(core_manager.instance(running_id).is_none());
        assert!(!config_dir.join(format!("{running_id}.toml")).exists());
        assert!(!config_dir.join(format!("{saved_id}.toml")).exists());
        assert!(core_manager.instance(protected_id).is_some());
        assert!(protected_path.is_file());
        let ids = mgr
            .handle_list_network_instance_ids(identify)
            .await
            .unwrap();
        assert!(!ids.disabled_inst_ids.contains(&saved_id.into()));
        assert!(ids.running_inst_ids.contains(&protected_id.into()));

        // The config-only endpoint must enforce the same loader permissions.
        use easytier::proto::api::manage::RemoveNetworkInstanceConfigRequest;
        let response = mgr
            .get_rpc_client(identify)
            .unwrap()
            .remove_network_instance_config(
                Default::default(),
                RemoveNetworkInstanceConfigRequest {
                    inst_ids: vec![protected_id.into()],

                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(response.remain_inst_ids, vec![protected_id.into()]);
        assert!(protected_path.is_file());

        client.shutdown().await;
        core_manager.retain_network_instances(&[]).await.unwrap();
        webhook_server.abort();
        std::fs::remove_dir_all(config_dir).unwrap();
    }

    #[tokio::test]
    async fn local_config_core_ignores_full_managed_config_reconcile() {
        let (webhook_config, webhook_server, _) = test_webhook_config().await;
        let mut mgr = ClientManager::new(
            Db::memory_db().await,
            None,
            HeartbeatPolicy::from_millis(0, 15_000).unwrap(),
            Arc::new(FeatureFlags::default()),
            webhook_config,
        );
        let config_server_addr = add_random_udp_listener(&mut mgr).await;
        let config_dir = std::env::temp_dir().join(format!(
            "easytier-web-local-config-full-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&config_dir).unwrap();

        let machine_id = uuid::Uuid::new_v4();
        let instance_id = uuid::Uuid::new_v4();
        let core_manager = local_config_instance_manager(config_dir.clone());
        let client =
            start_web_client_for_test(config_server_addr, machine_id, core_manager.clone()).await;
        let user_id = wait_for_validated_user(&mgr, machine_id).await;
        wait_for_local_config_capability(&mgr, user_id, machine_id).await;

        mgr.reconcile_managed_network_configs(
            user_id,
            machine_id,
            vec![managed_config(
                instance_id,
                initial_managed_network_config(instance_id),
            )],
            Some("local-full".to_string()),
            None,
        )
        .await
        .unwrap();

        assert_no_managed_config_state(&mgr, user_id, machine_id, instance_id).await;
        assert!(core_manager.instance(instance_id).is_none());
        assert!(!config_dir.join(format!("{instance_id}.toml")).exists());

        drop(client);
        webhook_server.abort();
        std::fs::remove_dir_all(config_dir).unwrap();
    }

    #[tokio::test]
    async fn local_config_core_ignores_managed_config_patch() {
        let (webhook_config, webhook_server, _) = test_webhook_config().await;
        let mut mgr = ClientManager::new(
            Db::memory_db().await,
            None,
            HeartbeatPolicy::from_millis(0, 15_000).unwrap(),
            Arc::new(FeatureFlags::default()),
            webhook_config,
        );
        let config_server_addr = add_random_udp_listener(&mut mgr).await;
        let config_dir = std::env::temp_dir().join(format!(
            "easytier-web-local-config-patch-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&config_dir).unwrap();

        let machine_id = uuid::Uuid::new_v4();
        let instance_id = uuid::Uuid::new_v4();
        let core_manager = local_config_instance_manager(config_dir.clone());
        let client =
            start_web_client_for_test(config_server_addr, machine_id, core_manager.clone()).await;
        let user_id = wait_for_validated_user(&mgr, machine_id).await;
        wait_for_local_config_capability(&mgr, user_id, machine_id).await;

        mgr.patch_managed_network_configs(
            user_id,
            machine_id,
            vec![managed_config(
                instance_id,
                updated_managed_network_config(instance_id),
            )],
            Vec::new(),
            "local-patch".to_string(),
            "missing-local-base".to_string(),
        )
        .await
        .unwrap();

        assert_no_managed_config_state(&mgr, user_id, machine_id, instance_id).await;
        assert!(core_manager.instance(instance_id).is_none());
        assert!(!config_dir.join(format!("{instance_id}.toml")).exists());

        drop(client);
        webhook_server.abort();
        std::fs::remove_dir_all(config_dir).unwrap();
    }

    #[tokio::test]
    async fn compact_runtime_preserves_unsupported_web_config_during_hot_patch() {
        let (webhook_config, webhook_server, _) = test_webhook_config().await;
        let mut mgr = ClientManager::new(
            Db::memory_db().await,
            None,
            HeartbeatPolicy::from_millis(0, 15_000).unwrap(),
            Arc::new(FeatureFlags::default()),
            webhook_config,
        );
        let config_server_addr = add_random_udp_listener(&mut mgr).await;

        let machine_id = uuid::Uuid::new_v4();
        let instance_id = uuid::Uuid::new_v4();
        let core_manager = Arc::new(native_compact_instance_manager_with_runtime(
            tokio::runtime::Handle::current(),
        ));
        let client =
            start_web_client_for_test(config_server_addr, machine_id, core_manager.clone()).await;
        let user_id = wait_for_validated_user(&mgr, machine_id).await;

        let desired = updated_managed_network_config(instance_id);
        let mut initial = desired.clone();
        initial["port_forwards"] = json!([]);
        mgr.reconcile_managed_network_configs(
            user_id,
            machine_id,
            vec![managed_config(instance_id, initial)],
            Some("compact-initial".to_string()),
            None,
        )
        .await
        .unwrap();
        wait_for_runtime_config(&core_manager, instance_id, |config| {
            config.network_name.as_deref() == Some("managed-updated")
                && config.port_forwards.is_empty()
        })
        .await;

        mgr.reconcile_managed_network_configs(
            user_id,
            machine_id,
            vec![managed_config(instance_id, desired)],
            Some("compact-patched".to_string()),
            Some("compact-initial".to_string()),
        )
        .await
        .unwrap();
        let patched = wait_for_runtime_config(&core_manager, instance_id, |config| {
            config.network_name.as_deref() == Some("managed-updated")
                && config.port_forwards.len() == 1
        })
        .await;

        assert_updated_runtime_config(&patched, instance_id);
        assert_eq!(
            mgr.db()
                .get_managed_config_revision((user_id, machine_id))
                .await
                .unwrap()
                .as_deref(),
            Some("compact-patched")
        );
        core_manager
            .delete_network_instances([instance_id])
            .await
            .unwrap();
        webhook_server.abort();
        drop(client);
    }

    #[tokio::test]
    async fn managed_web_config_revision_updates_running_core_config() {
        let (webhook_config, webhook_server, _) = test_webhook_config().await;
        let mut mgr = ClientManager::new(
            Db::memory_db().await,
            None,
            HeartbeatPolicy::from_millis(0, 15_000).unwrap(),
            Arc::new(FeatureFlags::default()),
            webhook_config,
        );
        let config_server_addr = add_random_udp_listener(&mut mgr).await;

        let machine_id = uuid::Uuid::new_v4();
        let instance_id = uuid::Uuid::new_v4();
        let core_manager = Arc::new(native_instance_manager());
        let client =
            start_web_client_for_test(config_server_addr, machine_id, core_manager.clone()).await;

        let user_id = wait_for_validated_user(&mgr, machine_id).await;
        mgr.reconcile_managed_network_configs(
            user_id,
            machine_id,
            vec![managed_config(
                instance_id,
                initial_managed_network_config(instance_id),
            )],
            Some("rev-initial".to_string()),
            None,
        )
        .await
        .unwrap();

        wait_for_runtime_config(&core_manager, instance_id, |config| {
            config.network_name.as_deref() == Some("managed-initial")
        })
        .await;
        wait_for_applied_revision(&mgr, user_id, machine_id, "rev-initial").await;

        // Runtime-only mutations do not change SQLite. Invalidate the Session
        // applied fence and verify the existing revision is fully reconciled
        // before a later targeted Patch may rely on it as a base.
        let mut drifted: NetworkConfig =
            serde_json::from_value(initial_managed_network_config(instance_id)).unwrap();
        drifted.network_name = Some("runtime-only-drift".to_string());
        mgr.handle_run_network_instance_with_source(
            (user_id, machine_id),
            drifted,
            false,
            ConfigSource::Web,
        )
        .await
        .unwrap();
        wait_for_runtime_config(&core_manager, instance_id, |config| {
            config.network_name.as_deref() == Some("runtime-only-drift")
        })
        .await;
        mgr.invalidate_applied_config_revision(user_id, machine_id)
            .await;
        wait_for_runtime_config(&core_manager, instance_id, |config| {
            config.network_name.as_deref() == Some("managed-initial")
        })
        .await;
        wait_for_applied_revision(&mgr, user_id, machine_id, "rev-initial").await;

        // Online revision update: web-owned running config is fully overwritten
        // when non-hot-patch flags such as enable_kcp_proxy change.
        mgr.reconcile_managed_network_configs(
            user_id,
            machine_id,
            vec![managed_config(
                instance_id,
                updated_managed_network_config(instance_id),
            )],
            Some("rev-updated".to_string()),
            Some("rev-initial".to_string()),
        )
        .await
        .unwrap();

        let updated = wait_for_runtime_config(&core_manager, instance_id, |config| {
            config.network_name.as_deref() == Some("managed-updated")
                && config.enable_kcp_proxy == Some(true)
                && config.port_forwards.len() == 1
        })
        .await;
        assert_updated_runtime_config(&updated, instance_id);

        assert_eq!(
            core_manager.config_source(instance_id),
            Some(easytier::common::config::ConfigSource::Web)
        );
        assert_eq!(
            mgr.db()
                .get_managed_config_revision((user_id, machine_id))
                .await
                .unwrap()
                .as_deref(),
            Some("rev-updated")
        );

        // Web DB loss path: clear web-owned config and revision, then simulate
        // the webhook re-posting the authoritative desired config. The already
        // connected session should receive the distinguishable re-delivered
        // revision without restarting.
        clear_managed_config_db(&mgr, user_id, machine_id, instance_id).await;
        assert!(
            mgr.db()
                .get_network_config((user_id, machine_id), &instance_id.to_string())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            mgr.db()
                .get_managed_config_revision((user_id, machine_id))
                .await
                .unwrap()
                .is_none()
        );

        mgr.reconcile_managed_network_configs(
            user_id,
            machine_id,
            vec![managed_config(
                instance_id,
                redelivered_managed_network_config(instance_id),
            )],
            Some("rev-webhook-redelivery".to_string()),
            None,
        )
        .await
        .unwrap();
        let redelivered = wait_for_runtime_config(&core_manager, instance_id, |config| {
            config.network_name.as_deref() == Some("managed-redelivered")
                && config.instance_recv_bps_limit == Some(654321)
        })
        .await;
        assert_eq!(
            redelivered.instance_id.as_deref(),
            Some(instance_id.to_string().as_str())
        );
        assert_eq!(
            redelivered.hostname.as_deref(),
            Some("managed-redelivered-host")
        );
        assert_eq!(
            redelivered.network_name.as_deref(),
            Some("managed-redelivered")
        );
        assert_eq!(redelivered.enable_kcp_proxy, Some(true));
        assert_eq!(redelivered.instance_recv_bps_limit, Some(654321));
        assert_eq!(
            core_manager.config_source(instance_id),
            Some(easytier::common::config::ConfigSource::Web)
        );
        assert_eq!(
            mgr.db()
                .get_managed_config_revision((user_id, machine_id))
                .await
                .unwrap()
                .as_deref(),
            Some("rev-webhook-redelivery")
        );

        // Reconnect path: a fresh core manager has no local runtime state, so
        // the new session must replay the managed config persisted in web DB.
        drop(client);
        let reconnected_core_manager = Arc::new(native_instance_manager());
        let reconnected_client = start_web_client_for_test(
            config_server_addr,
            machine_id,
            reconnected_core_manager.clone(),
        )
        .await;
        wait_for_validated_user(&mgr, machine_id).await;
        let replayed = wait_for_runtime_config(&reconnected_core_manager, instance_id, |config| {
            config.network_name.as_deref() == Some("managed-redelivered")
                && config.instance_recv_bps_limit == Some(654321)
        })
        .await;
        assert_eq!(
            replayed.network_name.as_deref(),
            Some("managed-redelivered")
        );
        assert_eq!(replayed.enable_kcp_proxy, Some(true));
        assert_eq!(replayed.instance_recv_bps_limit, Some(654321));

        core_manager
            .delete_network_instances([instance_id])
            .await
            .unwrap();
        reconnected_core_manager
            .delete_network_instances([instance_id])
            .await
            .unwrap();
        drop(reconnected_client);
        webhook_server.abort();
    }

    #[tokio::test]
    async fn webhook_reject_disconnects_and_revalidates_after_reconnect() {
        let webhook_state = TestWebhookState::with_blocked_second_validate([false, true]);
        let (webhook_config, webhook_server, webhook_state) =
            test_webhook_config_with_state(webhook_state).await;
        let mut mgr = ClientManager::new(
            Db::memory_db().await,
            None,
            HeartbeatPolicy::from_millis(0, 15_000).unwrap(),
            Arc::new(FeatureFlags::default()),
            webhook_config,
        );
        let config_server_addr = add_random_udp_listener(&mut mgr).await;
        let machine_id = uuid::Uuid::new_v4();
        let core_manager = Arc::new(native_instance_manager());
        let client =
            start_web_client_for_test(config_server_addr, machine_id, core_manager.clone()).await;

        let first_session_urls = wait_for_session_urls(&mgr).await;
        wait_for_validate_count(&webhook_state, 1).await;
        wait_for_validate_count(&webhook_state, 2).await;
        assert!(
            mgr.list_sessions().await.is_empty(),
            "invalid validate-token response must not authorize the session"
        );

        webhook_state.allow_second_validate();
        let user_id = wait_for_validated_user(&mgr, machine_id).await;
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                let reconnected = mgr
                    .client_sessions
                    .iter()
                    .any(|entry| !first_session_urls.iter().any(|url| url == entry.key()));
                if reconnected {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();

        assert!(
            client.is_connected(),
            "web client should reconnect after invalid session heartbeat failure"
        );
        assert!(webhook_state.validate_count() >= 2);
        assert!(
            mgr.get_session_by_machine_id(user_id, &machine_id)
                .is_some()
        );

        webhook_server.abort();
    }
}
