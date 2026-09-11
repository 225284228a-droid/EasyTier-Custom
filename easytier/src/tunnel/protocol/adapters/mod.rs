use std::sync::Arc;

use easytier_core::connectivity::protocol::{ClientProtocolUpgrader, ServerProtocolUpgrader};

use crate::{common::global_ctx::ArcGlobalCtx, socket::tcp::RuntimeTcpSocket};

#[cfg(feature = "quic")]
mod http3;
#[cfg(feature = "quic")]
mod quic;
#[cfg(feature = "websocket")]
mod websocket;
#[cfg(feature = "wireguard")]
mod wireguard;

pub(super) type ClientAdapter = Arc<dyn ClientProtocolUpgrader<RuntimeTcpSocket>>;
pub(super) type ServerAdapter = Arc<dyn ServerProtocolUpgrader<RuntimeTcpSocket>>;

fn apply_sni_override(mut url: url::Url, sni: &str) -> url::Url {
    let sni = sni.trim();
    let query = url
        .query_pairs()
        .filter(|(key, _)| key != "sni")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    url.set_query(None);
    {
        let mut pairs = url.query_pairs_mut();
        pairs.extend_pairs(query);
        if !sni.is_empty() {
            pairs.append_pair("sni", sni);
        }
    }
    url
}

pub(super) fn client_adapters(global_ctx: &ArcGlobalCtx) -> Vec<ClientAdapter> {
    let _ = global_ctx;
    [
        #[cfg(feature = "websocket")]
        websocket::client_adapter(global_ctx),
        #[cfg(feature = "wireguard")]
        wireguard::client_adapter(global_ctx),
        #[cfg(feature = "quic")]
        quic::client_adapter(global_ctx),
        #[cfg(feature = "quic")]
        http3::client_adapter(global_ctx),
    ]
    .into_iter()
    .collect()
}

pub(super) fn server_adapters(global_ctx: &ArcGlobalCtx) -> Vec<ServerAdapter> {
    let _ = global_ctx;
    [
        #[cfg(feature = "websocket")]
        websocket::server_adapter(global_ctx),
        #[cfg(feature = "wireguard")]
        wireguard::server_adapter(global_ctx),
        #[cfg(feature = "quic")]
        quic::server_adapter(global_ctx),
        #[cfg(feature = "quic")]
        http3::server_adapter(global_ctx),
    ]
    .into_iter()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::apply_sni_override;

    #[test]
    fn global_sni_replaces_per_url_sni_and_preserves_other_query_params() {
        let url = "wss://192.0.2.1:443/path?token=abc&sni=old.example"
            .parse()
            .unwrap();
        let updated = apply_sni_override(url, "www.cloudflare.com");

        assert_eq!(
            updated.as_str(),
            "wss://192.0.2.1/path?token=abc&sni=www.cloudflare.com"
        );

        let fallback = apply_sni_override(
            "wss://192.0.2.1/path?token=abc&sni=old.example"
                .parse()
                .unwrap(),
            "",
        );
        assert_eq!(fallback.as_str(), "wss://192.0.2.1/path?token=abc");
    }
}
