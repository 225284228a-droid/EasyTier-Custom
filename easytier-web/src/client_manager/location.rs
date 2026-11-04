//! Session and network location lookup plus the GeoIP city-cache plumbing.
//!
//! Split out of `mod.rs` so the upstream file stays close to its original
//! shape; the logic itself is unchanged.

use std::{sync::Arc, time::Duration};

use easytier::proto::{
    api::manage::{CollectNetworkInfoRequest, CollectNetworkInfoResponse, MyNodeInfo},
    rpc_types::controller::BaseController,
};
use tokio::task::JoinSet;

use super::{ClientManager, Session, network_info_cache, session::Location};
use crate::{
    db::UserIdInDb,
    geolocation::{CityGeoCache, node_public_ips},
};

impl ClientManager {
    pub fn network_info_cache(&self) -> &network_info_cache::NetworkInfoCache {
        &self.network_info_cache
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
}

impl ClientManager {
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

    /// Location for a set of public addresses of one node, IPv4 first.
    pub async fn resolve_ips_location(&self, ips: &[std::net::IpAddr]) -> Option<Location> {
        Self::resolve_node_ips(
            ips,
            self.city_geo_cache.as_ref(),
            self.geoip_db.clone(),
            true,
        )
        .await
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

    pub(super) async fn resolve_node_ips(
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

    pub(super) fn snapshot_public_ips(
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

    pub(super) async fn apply_network_location_snapshot(
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
    pub(super) async fn cached_location(
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
}
