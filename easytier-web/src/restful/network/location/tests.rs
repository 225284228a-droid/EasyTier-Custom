use easytier::proto::api::manage::{CollectNetworkInfoResponse, MyNodeInfo};

use crate::client_manager::ClientManager;

use super::NetworkApi;

#[test]
fn node_public_ip_prefers_network_stun_address() {
    let node = MyNodeInfo {
        stun_info: Some(easytier::proto::common::StunInfo {
            public_ip: vec!["203.0.113.10".to_string()],
            ..Default::default()
        }),
        ips: Some(easytier::proto::peer_rpc::GetIpListResponse {
            public_ipv4: Some(std::net::Ipv4Addr::new(198, 51, 100, 20).into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        NetworkApi::node_public_ip(&node),
        Some("203.0.113.10".parse().unwrap())
    );
}

#[test]
fn node_public_ip_displays_ipv4_even_when_stun_lists_ipv6_first() {
    let mut node = MyNodeInfo {
        stun_info: Some(easytier::proto::common::StunInfo {
            public_ip: vec!["2001:4860:4860::8888".into(), "::ffff:8.8.8.8".into()],
            ..Default::default()
        }),
        ..Default::default()
    };
    let expected = Some("8.8.8.8".parse().unwrap());
    assert_eq!(NetworkApi::node_public_ip(&node), expected);
    node.stun_info.as_mut().unwrap().public_ip.reverse();
    assert_eq!(NetworkApi::node_public_ip(&node), expected);
    node.stun_info
        .as_mut()
        .unwrap()
        .public_ip
        .retain(|ip| ip.contains("2001:"));
    node.ips = Some(easytier::proto::peer_rpc::GetIpListResponse {
        public_ipv4: Some(std::net::Ipv4Addr::new(8, 8, 8, 8).into()),
        ..Default::default()
    });
    assert_eq!(NetworkApi::node_public_ip(&node), expected);
}

#[tokio::test]
async fn network_annotation_keeps_ipv4_display_when_same_node_ipv6_supplies_city() {
    use easytier::proto::api::manage::{NetworkInstanceRunningInfo, NetworkInstanceRunningInfoMap};
    use std::sync::Arc;

    let cache = crate::geolocation::CityGeoCache::test_memory(crate::geolocation::Options {
        offline: true,
        ..Default::default()
    })
    .await;
    cache
        .test_seed(
            "2001:4860:4860::8888".parse().unwrap(),
            &crate::geolocation::CityLocation {
                country: "New Zealand".into(),
                city: "Auckland".into(),
                region: None,
                latitude: -36.85,
                longitude: 174.76,
                accuracy_radius_km: None,
            },
            chrono::Utc::now().timestamp(),
        )
        .await;
    let mut manager = ClientManager::new(
        crate::db::Db::memory_db().await,
        None,
        crate::client_manager::HeartbeatPolicy::default(),
        Arc::new(crate::FeatureFlags::default()),
        Arc::new(crate::webhook::WebhookConfig::new(
            None, None, None, None, None,
        )),
    );
    manager.set_city_geo_cache(cache);
    let node = MyNodeInfo {
        stun_info: Some(easytier::proto::common::StunInfo {
            public_ip: vec!["2001:4860:4860::8888".into(), "1.1.1.1".into()],
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut response = CollectNetworkInfoResponse {
        info: Some(NetworkInstanceRunningInfoMap {
            map: [(
                "instance".to_string(),
                NetworkInstanceRunningInfo {
                    running: true,
                    my_node_info: Some(node),
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
        }),
    };
    NetworkApi::annotate_network_locations(&manager, 0, uuid::Uuid::new_v4(), &mut response).await;
    let location = response.info.as_ref().unwrap().map["instance"]
        .node_location
        .as_ref()
        .unwrap();
    assert_eq!(location.public_ip, "1.1.1.1");
    assert_eq!(location.city, "Auckland");
    assert_eq!(location.latitude, Some(-36.85));
    assert_eq!(location.longitude, Some(174.76));
}

#[test]
fn conn_remote_ip_prefers_the_resolved_address_of_a_domain_peer() {
    let tunnel = |remote: &str, resolved: Option<&str>| easytier::proto::common::TunnelInfo {
        remote_addr: Some(remote.parse::<url::Url>().unwrap().into()),
        resolved_remote_addr: resolved.map(|url| url.parse::<url::Url>().unwrap().into()),
        ..Default::default()
    };
    assert_eq!(
        NetworkApi::conn_remote_ip(&tunnel(
            "tcp://public.example.com:11010",
            Some("tcp://203.0.113.7:11010"),
        )),
        Some("203.0.113.7".parse().unwrap())
    );
    // Inbound connections and older nodes only report the observed address.
    assert_eq!(
        NetworkApi::conn_remote_ip(&tunnel("tcp://198.51.100.4:50000", None)),
        Some("198.51.100.4".parse().unwrap())
    );
    assert_eq!(
        NetworkApi::conn_remote_ip(&tunnel("udp://[2001:db8::1]:11010", None)),
        Some("2001:db8::1".parse().unwrap())
    );
    assert_eq!(
        NetworkApi::conn_remote_ip(&tunnel("wss://203.0.113.9:443/ws", None)),
        Some("203.0.113.9".parse().unwrap())
    );
    assert_eq!(
        NetworkApi::conn_remote_ip(&tunnel("wss://public.example.com/ws", None)),
        None
    );
}

#[test]
fn node_public_ip_ignores_unusable_stun_addresses() {
    let node = MyNodeInfo {
        stun_info: Some(easytier::proto::common::StunInfo {
            public_ip: vec![
                "proxy.example.com".to_string(),
                "0.0.0.0".to_string(),
                "127.0.0.1".to_string(),
                "10.0.0.1".to_string(),
                "100.64.0.1".to_string(),
                "fd00::1".to_string(),
            ],
            ..Default::default()
        }),
        ips: Some(easytier::proto::peer_rpc::GetIpListResponse {
            public_ipv4: Some(std::net::Ipv4Addr::new(198, 51, 100, 20).into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        NetworkApi::node_public_ip(&node),
        Some("198.51.100.20".parse().unwrap())
    );
}

#[test]
fn node_public_ip_rejects_default_or_loopback_node_addresses() {
    let empty = MyNodeInfo {
        ips: Some(easytier::proto::peer_rpc::GetIpListResponse {
            public_ipv4: Some(std::net::Ipv4Addr::UNSPECIFIED.into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(NetworkApi::node_public_ip(&empty), None);

    let local = MyNodeInfo {
        ips: Some(easytier::proto::peer_rpc::GetIpListResponse {
            public_ipv4: Some(std::net::Ipv4Addr::LOCALHOST.into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(NetworkApi::node_public_ip(&local), None);
}
