use std::sync::Arc;

use async_trait::async_trait;
use easytier_core::{
    connectivity::{
        protocol::{
            ClientProtocolUpgrader, ServerProtocolAdmission, ServerProtocolUpgrade,
            ServerProtocolUpgrader,
        },
        transport::ConnectedTransport,
    },
    socket::udp::UdpSession,
    tunnel::Tunnel,
};

use crate::{
    common::global_ctx::ArcGlobalCtx,
    socket::tcp::RuntimeTcpSocket,
    tunnel::http3::{Http3AcceptedSession, upgrade_connected},
};

use super::{ClientAdapter, ServerAdapter, apply_sni_override};

#[derive(Default)]
struct Http3Adapter {
    global_ctx: Option<ArcGlobalCtx>,
}

pub(super) fn client_adapter(global_ctx: &ArcGlobalCtx) -> ClientAdapter {
    Arc::new(Http3Adapter {
        global_ctx: Some(global_ctx.clone()),
    })
}

pub(super) fn server_adapter(_global_ctx: &ArcGlobalCtx) -> ServerAdapter {
    Arc::new(Http3Adapter::default())
}

#[async_trait]
impl ClientProtocolUpgrader<RuntimeTcpSocket> for Http3Adapter {
    fn supports_scheme(&self, scheme: &str) -> bool {
        scheme == "http3"
    }

    async fn upgrade_client(
        &self,
        connected: ConnectedTransport<RuntimeTcpSocket>,
        requested_url: url::Url,
    ) -> anyhow::Result<Box<dyn Tunnel>> {
        let ConnectedTransport::Udp(session) = connected else {
            anyhow::bail!("http3 protocol requires a UDP session");
        };
        let sni = self
            .global_ctx
            .as_ref()
            .map(|global_ctx| global_ctx.config.get_sni())
            .unwrap_or_default();
        let requested_url = apply_sni_override(requested_url, &sni);
        Ok(upgrade_connected(session, requested_url).await?)
    }
}

#[async_trait]
impl ServerProtocolUpgrader<RuntimeTcpSocket> for Http3Adapter {
    fn supports_scheme(&self, scheme: &str) -> bool {
        scheme == "http3"
    }

    async fn upgrade_tcp(
        &self,
        _socket: RuntimeTcpSocket,
        _local_url: url::Url,
    ) -> anyhow::Result<ServerProtocolUpgrade> {
        anyhow::bail!("unsupported native TCP server protocol upgrader: http3")
    }

    async fn upgrade_udp(
        &self,
        session: UdpSession,
        local_url: url::Url,
        admission: Option<ServerProtocolAdmission>,
    ) -> anyhow::Result<ServerProtocolUpgrade> {
        let admission =
            admission.ok_or_else(|| anyhow::anyhow!("http3 server admission permit is missing"))?;
        Ok(ServerProtocolUpgrade::Acceptor(Box::new(
            Http3AcceptedSession::new(session, local_url, admission)?,
        )))
    }

    async fn upgrade_byte_stream(
        &self,
        _socket: RuntimeTcpSocket,
        _local_url: url::Url,
        _remote_url: Option<url::Url>,
    ) -> anyhow::Result<ServerProtocolUpgrade> {
        anyhow::bail!("unsupported native byte-stream server protocol upgrader: http3")
    }
}
