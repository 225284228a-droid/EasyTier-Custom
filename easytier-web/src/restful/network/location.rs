//! Location and topology annotation for network instance info.
//!
//! These helpers decide which public address identifies a node, translate it
//! into a cached city location, and annotate every instance of a collected
//! network info response with node and peer locations.

use easytier::proto::api::manage::{
    CollectNetworkInfoResponse, MyNodeInfo, NetworkInstanceRunningInfo, NodeLocation,
    PeerNodeLocation,
};

use crate::client_manager::ClientManager;
use crate::client_manager::session::Location;
use crate::db::UserIdInDb;

use super::NetworkApi;

impl NetworkApi {
    #[cfg(test)]
    pub(super) fn usable_node_ip(ip: &std::net::IpAddr) -> bool {
        crate::geolocation::usable_node_ip(ip)
    }

    pub(super) fn node_public_ip(node: &MyNodeInfo) -> Option<std::net::IpAddr> {
        crate::geolocation::node_public_ip(node)
    }

    pub(super) async fn annotate_network_locations(
        client_mgr: &ClientManager,
        user_id: UserIdInDb,
        machine_id: uuid::Uuid,
        response: &mut CollectNetworkInfoResponse,
    ) {
        let Some(info) = response.info.as_mut() else {
            return;
        };

        let mut details: Vec<_> = info.map.iter_mut().collect();
        details.sort_by(|(left_id, left), (right_id, right)| {
            let ipv6 = |detail: &easytier::proto::api::manage::NetworkInstanceRunningInfo| {
                detail
                    .my_node_info
                    .as_ref()
                    .and_then(Self::node_public_ip)
                    .is_none_or(|ip| ip.is_ipv6())
            };
            ipv6(left)
                .cmp(&ipv6(right))
                .then_with(|| left_id.cmp(right_id))
        });
        let mut primary_cached = false;
        for (_, detail) in details {
            if !detail.running {
                continue;
            }
            let Some(node) = detail.my_node_info.as_ref() else {
                continue;
            };
            if let Some((ip, location)) = client_mgr.resolve_node_location(node).await {
                if !primary_cached {
                    client_mgr
                        .cache_network_location(user_id, machine_id, node, location.clone())
                        .await;
                    primary_cached = true;
                }
                detail.node_location = Some(Self::node_location_pb(ip, location));
            }
            detail.peer_locations = Self::resolve_peer_locations(client_mgr, detail).await;
        }
    }

    /// Locate every remote peer in the route table, including peers whose
    /// machines are not connected to this console.
    pub(super) async fn resolve_peer_locations(
        client_mgr: &ClientManager,
        detail: &NetworkInstanceRunningInfo,
    ) -> Vec<PeerNodeLocation> {
        let local_peer_id = detail.my_node_info.as_ref().map(|node| node.peer_id);
        let lookups = detail
            .routes
            .iter()
            .filter(|route| route.peer_id != 0 && Some(route.peer_id) != local_peer_id)
            .map(|route| {
                let conn_ips = detail
                    .peers
                    .iter()
                    .filter(|peer| peer.peer_id == route.peer_id)
                    .flat_map(|peer| &peer.conns)
                    .filter(|conn| !conn.is_closed)
                    .filter_map(|conn| Self::conn_remote_ip(conn.tunnel.as_ref()?));
                let ips = crate::geolocation::public_ips(route.stun_info.as_ref(), conn_ips);
                let peer_id = route.peer_id;
                async move {
                    let ip = *ips.first()?;
                    let location = client_mgr.resolve_ips_location(&ips).await?;
                    Some(PeerNodeLocation {
                        peer_id,
                        location: Some(Self::node_location_pb(ip, location)),
                    })
                }
            });
        futures::future::join_all(lookups)
            .await
            .into_iter()
            .flatten()
            .collect()
    }

    /// The address a connection actually reached. A peer added by domain
    /// name, typically a public server, only has an IP in the resolved
    /// address; older nodes do not report one.
    pub(super) fn conn_remote_ip(
        tunnel: &easytier::proto::common::TunnelInfo,
    ) -> Option<std::net::IpAddr> {
        [&tunnel.resolved_remote_addr, &tunnel.remote_addr]
            .into_iter()
            .find_map(|url| Self::url_ip(url.as_ref()?))
    }

    pub(super) fn url_ip(url: &easytier::proto::common::Url) -> Option<std::net::IpAddr> {
        match url::Url::parse(&url.url).ok()?.host()? {
            url::Host::Ipv4(ip) => Some(ip.into()),
            url::Host::Ipv6(ip) => Some(ip.into()),
            // Hosts of non-special schemes such as tcp:// and udp:// are
            // opaque to the URL parser, so an IPv4 address arrives here.
            url::Host::Domain(host) => host.parse().ok(),
        }
    }

    pub(super) fn node_location_pb(ip: std::net::IpAddr, location: Location) -> NodeLocation {
        NodeLocation {
            public_ip: ip.to_string(),
            country: location.country,
            city: location.city.unwrap_or_default(),
            region: location.region.unwrap_or_default(),
            latitude: location.latitude,
            longitude: location.longitude,
        }
    }
}

#[cfg(test)]
mod tests;
