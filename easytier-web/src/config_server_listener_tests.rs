use super::{create_config_server_listeners, parse_config_server_listener_urls};

#[tokio::test]
async fn tcp_and_udp_candidates_do_not_depend_on_default_routes() {
    for protocol in ["tcp", "udp"] {
        let urls = vec![format!("{protocol}://0.0.0.0:22020").parse().unwrap()];
        let listeners = create_config_server_listeners(&urls).await;
        assert_eq!(listeners.len(), 2);
        assert_eq!(listeners[0].0.as_str(), format!("{protocol}://[::]:22020"));
        assert_eq!(
            listeners[1].0.as_str(),
            format!("{protocol}://0.0.0.0:22020")
        );
    }
}

#[tokio::test]
async fn websocket_listeners_preserve_explicit_hosts() {
    for protocol in ["ws", "wss"] {
        let urls = vec![format!("{protocol}://0.0.0.0:22020").parse().unwrap()];
        let listeners = create_config_server_listeners(&urls).await;
        assert_eq!(listeners.len(), 1);
        assert_eq!(
            listeners[0].0.as_str(),
            format!("{protocol}://0.0.0.0:22020/")
        );
    }
}

#[test]
fn invalid_protocol_is_rejected() {
    for protocol in ["invalid", "quic", "http3"] {
        assert!(
            parse_config_server_listener_urls(&[format!("{protocol}://0.0.0.0:22020")]).is_err()
        );
    }
}

#[tokio::test]
async fn ipv4_listeners_bind_even_when_ipv6_is_unavailable() {
    for protocol in ["tcp", "udp", "ws", "wss"] {
        let urls = vec![format!("{protocol}://0.0.0.0:0").parse().unwrap()];
        let (_, mut ipv4) = create_config_server_listeners(&urls).await.pop().unwrap();
        // This also runs in an isolated IPv4-only network namespace without
        // default routes, so construction and binding must not probe the WAN.
        ipv4.listen().await.unwrap();
    }
}
