use super::*;

async fn listener_test_manager() -> client_manager::ClientManager {
    client_manager::ClientManager::new(
        db::Db::memory_db().await,
        None,
        client_manager::HeartbeatPolicy::default(),
        Arc::new(FeatureFlags::default()),
        Arc::new(webhook::WebhookConfig::new(None, None, None, None, None)),
    )
}

#[tokio::test]
async fn enrollment_endpoint_uses_actual_bound_protocol_and_port() {
    for scheme in ["tcp", "udp", "ws", "wss"] {
        let mut manager = listener_test_manager().await;
        let endpoint = bind_config_server_listeners(
            &mut manager,
            &[format!("{scheme}://127.0.0.1:0").parse().unwrap()],
            None,
        )
        .await
        .unwrap();
        assert_eq!(endpoint.scheme(), scheme);
        assert_ne!(endpoint.port_or_known_default(), Some(0));
        assert!(endpoint.port_or_known_default().is_some());
    }
}

#[tokio::test]
async fn enrollment_endpoint_keeps_configured_listener_order() {
    let mut manager = listener_test_manager().await;
    let endpoint = bind_config_server_listeners(
        &mut manager,
        &parse_entries(&["ws://127.0.0.1:0", "tcp://127.0.0.1:0"]).unwrap(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(endpoint.scheme(), "ws");
    tokio::net::TcpStream::connect(("127.0.0.1", endpoint.port().unwrap()))
        .await
        .unwrap();
}

#[tokio::test]
async fn enrollment_endpoint_skips_failed_listeners() {
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let unavailable = format!("tcp://{}", occupied.local_addr().unwrap());
    let mut manager = listener_test_manager().await;
    let endpoint = bind_config_server_listeners(
        &mut manager,
        &parse_entries(&[&unavailable, "ws://127.0.0.1:0", "udp://127.0.0.1:0"]).unwrap(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(endpoint.scheme(), "ws");
    assert_ne!(endpoint.port(), Some(0));
    assert!(
        bind_config_server_listeners(&mut manager, &[], None)
            .await
            .is_err()
    );
}

fn parse_entries(entries: &[&str]) -> anyhow::Result<Vec<url::Url>> {
    parse_config_server_listener_urls(
        &entries
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<String>>(),
    )
}

#[test]
fn parses_repeated_comma_separated_and_mixed_protocol_urls() {
    let urls = parse_entries(&[
        "udp://0.0.0.0:22020,tcp://0.0.0.0:22021",
        "  ws://[::]:22022  ",
        "",
    ])
    .unwrap();
    assert_eq!(urls.len(), 3);
    assert_eq!(urls[0].scheme(), "udp");
    assert_eq!(urls[0].port(), Some(22020));
    assert_eq!(urls[1].scheme(), "tcp");
    assert_eq!(urls[1].port(), Some(22021));
    assert_eq!(urls[2].scheme(), "ws");
    assert_eq!(urls[2].port(), Some(22022));
}

#[test]
fn rejects_unsupported_protocol_and_missing_host() {
    assert!(parse_entries(&["quic://0.0.0.0:22020"]).is_err());
    assert!(parse_entries(&["udp://:22020"]).is_err());
    assert!(parse_entries(&["not a url"]).is_err());
    assert!(parse_entries(&["udp://0.0.0.0:22020", "tcp://bad url"]).is_err());
}

#[test]
fn detects_wildcard_hosts_for_dual_stack_expansion() {
    let v4_wildcard = "udp://0.0.0.0:22020".parse::<url::Url>().unwrap();
    let v6_wildcard = "tcp://[::]:22020".parse::<url::Url>().unwrap();
    let specific_v4 = "tcp://192.168.1.10:22020".parse::<url::Url>().unwrap();
    let specific_v6 = "tcp://[::1]:22020".parse::<url::Url>().unwrap();
    assert!(is_wildcard_host(&v4_wildcard));
    assert!(is_wildcard_host(&v6_wildcard));
    assert!(!is_wildcard_host(&specific_v4));
    assert!(!is_wildcard_host(&specific_v6));
}

#[test]
fn fills_default_port_when_missing() {
    let no_port = "udp://0.0.0.0".parse::<url::Url>().unwrap();
    assert_eq!(
        normalize_listener_url(&no_port).port(),
        Some(CONFIG_SERVER_DEFAULT_PORT)
    );
    let with_port = "wss://0.0.0.0:22023".parse::<url::Url>().unwrap();
    assert_eq!(normalize_listener_url(&with_port).port(), Some(22023));
}

#[test]
fn detects_explicit_scheme_default_ports_in_raw_urls() {
    assert!(raw_has_explicit_port("wss://host:443", 443));
    assert!(raw_has_explicit_port("ws://host:80/path?q=1", 80));
    assert!(raw_has_explicit_port("wss://[2001:db8::1]:443/x", 443));
    assert!(raw_has_explicit_port("wss://user@host:443", 443));
    assert!(!raw_has_explicit_port("wss://host", 443));
    assert!(!raw_has_explicit_port("wss://host:8443", 443));
    assert!(!raw_has_explicit_port("wss://[::1]", 443));
    assert!(!raw_has_explicit_port("wss://[::1]:", 443));
}
